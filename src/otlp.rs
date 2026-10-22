//! Minimal OTLP/JSON trace decoding.
//!
//! We deliberately do not pull in `opentelemetry-proto`: the dashboard only
//! needs a handful of span fields, and parsing straight into flat rows avoids
//! an intermediate protobuf object graph on the hot ingest path.
//!
//! Field names follow the OTLP JSON mapping (`resourceSpans`, `traceId`,
//! `startTimeUnixNano` …). Trace and span ids are hex, as the OTLP/JSON spec
//! requires. Enums are accepted as integers or their proto names.

use serde::Deserialize;

// ============================================================================
// Wire types
// ============================================================================

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTraceServiceRequest {
    #[serde(default)]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSpans {
    #[serde(default)]
    pub resource: Option<Resource>,
    #[serde(default)]
    pub scope_spans: Vec<ScopeSpans>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeSpans {
    #[serde(default)]
    pub scope: Option<InstrumentationScope>,
    #[serde(default)]
    pub spans: Vec<Span>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentationScope {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub span_id: String,
    #[serde(default)]
    pub parent_span_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: EnumField,
    #[serde(default)]
    pub start_time_unix_nano: U64Field,
    #[serde(default)]
    pub end_time_unix_nano: U64Field,
    #[serde(default)]
    pub attributes: Vec<KeyValue>,
    #[serde(default)]
    pub status: Option<Status>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(default)]
    pub code: EnumField,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: Option<AnyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnyValue {
    pub string_value: Option<String>,
    pub bool_value: Option<bool>,
    pub int_value: Option<I64Field>,
    pub double_value: Option<f64>,
    pub bytes_value: Option<String>,
    pub array_value: Option<ArrayValue>,
    pub kvlist_value: Option<KeyValueList>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrayValue {
    #[serde(default)]
    pub values: Vec<AnyValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValueList {
    #[serde(default)]
    pub values: Vec<KeyValue>,
}

/// Proto `uint64` arrives as a JSON string; some exporters send a number.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum U64Field {
    Num(u64),
    Str(String),
}

impl Default for U64Field {
    fn default() -> Self {
        Self::Num(0)
    }
}

impl U64Field {
    pub fn get(&self) -> Option<u64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Str(s) => s.parse().ok(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum I64Field {
    Num(i64),
    Str(String),
}

impl I64Field {
    pub fn get(&self) -> Option<i64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Str(s) => s.parse().ok(),
        }
    }
}

/// Proto enums are integers in OTLP/JSON, but the canonical proto3 JSON
/// mapping also allows the enum name (`SPAN_KIND_SERVER`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EnumField {
    Num(i32),
    Name(String),
}

impl Default for EnumField {
    fn default() -> Self {
        Self::Num(0)
    }
}

impl EnumField {
    fn resolve(&self, names: &[&str]) -> i32 {
        match self {
            Self::Num(n) => *n,
            Self::Name(name) => names
                .iter()
                .position(|candidate| candidate.eq_ignore_ascii_case(name))
                .map(|index| index as i32)
                .unwrap_or(0),
        }
    }

    pub fn span_kind(&self) -> i32 {
        self.resolve(&[
            "SPAN_KIND_UNSPECIFIED",
            "SPAN_KIND_INTERNAL",
            "SPAN_KIND_SERVER",
            "SPAN_KIND_CLIENT",
            "SPAN_KIND_PRODUCER",
            "SPAN_KIND_CONSUMER",
        ])
    }

    pub fn status_code(&self) -> i32 {
        self.resolve(&["STATUS_CODE_UNSET", "STATUS_CODE_OK", "STATUS_CODE_ERROR"])
    }
}

// ============================================================================
// Flattened rows
// ============================================================================

pub const UNKNOWN_SERVICE: &str = "unknown_service";
pub const SERVICE_NAME_ATTR: &str = "service.name";
const TRACE_ID_HEX_LEN: usize = 32;
const SPAN_ID_HEX_LEN: usize = 16;

/// One span, ready to insert. Ids are lowercase hex.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRow {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub service_name: String,
    pub scope_name: Option<String>,
    pub name: String,
    pub kind: i32,
    pub start_ns: i64,
    pub end_ns: i64,
    pub status_code: i32,
    pub status_message: Option<String>,
    /// Span attributes as a JSON object, serialized once at ingest time.
    pub attributes_json: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Flattened {
    pub rows: Vec<SpanRow>,
    pub rejected: u64,
}

/// Flatten an export request into insertable rows, counting spans whose ids
/// or timestamps are unusable instead of failing the whole batch.
pub fn flatten(request: ExportTraceServiceRequest) -> Flattened {
    let mut out = Flattened::default();

    for resource_spans in request.resource_spans {
        let service_name = resource_spans
            .resource
            .as_ref()
            .and_then(|resource| {
                resource.attributes.iter().find_map(|kv| {
                    (kv.key == SERVICE_NAME_ATTR)
                        .then(|| kv.value.as_ref().and_then(|v| v.string_value.clone()))
                        .flatten()
                })
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| UNKNOWN_SERVICE.to_string());

        for scope_spans in resource_spans.scope_spans {
            let scope_name = scope_spans
                .scope
                .as_ref()
                .map(|scope| scope.name.clone())
                .filter(|name| !name.is_empty());

            for span in scope_spans.spans {
                match flatten_span(span, &service_name, scope_name.as_deref()) {
                    Some(row) => out.rows.push(row),
                    None => out.rejected += 1,
                }
            }
        }
    }

    out
}

fn flatten_span(span: Span, service_name: &str, scope_name: Option<&str>) -> Option<SpanRow> {
    let trace_id = normalize_id(&span.trace_id, TRACE_ID_HEX_LEN)?;
    let span_id = normalize_id(&span.span_id, SPAN_ID_HEX_LEN)?;
    let parent_span_id = normalize_id(&span.parent_span_id, SPAN_ID_HEX_LEN);

    let start_ns = i64::try_from(span.start_time_unix_nano.get()?).ok()?;
    let end_ns = i64::try_from(span.end_time_unix_nano.get()?).ok()?;
    if end_ns < start_ns {
        return None;
    }

    let (status_code, status_message) = span
        .status
        .map(|status| {
            let message = (!status.message.is_empty()).then_some(status.message);
            (status.code.status_code(), message)
        })
        .unwrap_or((0, None));

    let attributes = attributes_to_json(span.attributes);
    let attributes_json = serde_json::to_string(&attributes).ok()?;

    Some(SpanRow {
        trace_id,
        span_id,
        parent_span_id,
        service_name: service_name.to_string(),
        scope_name: scope_name.map(str::to_string),
        name: span.name,
        kind: span.kind.span_kind(),
        start_ns,
        end_ns,
        status_code,
        status_message,
        attributes_json,
    })
}

/// Accept exactly `len` hex characters; return them lowercased. The all-zero
/// id is "absent" in OTLP and is treated as missing.
fn normalize_id(raw: &str, len: usize) -> Option<String> {
    if raw.len() != len || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if raw.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

pub fn attributes_to_json(attributes: Vec<KeyValue>) -> serde_json::Value {
    let map = attributes
        .into_iter()
        .filter(|kv| !kv.key.is_empty())
        .map(|kv| {
            (
                kv.key,
                kv.value
                    .map(any_value_to_json)
                    .unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(map)
}

fn any_value_to_json(value: AnyValue) -> serde_json::Value {
    use serde_json::Value;
    if let Some(s) = value.string_value {
        return Value::String(s);
    }
    if let Some(b) = value.bool_value {
        return Value::Bool(b);
    }
    if let Some(i) = value.int_value.as_ref().and_then(I64Field::get) {
        return Value::from(i);
    }
    if let Some(d) = value.double_value {
        return serde_json::Number::from_f64(d)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    if let Some(bytes) = value.bytes_value {
        return Value::String(bytes);
    }
    if let Some(array) = value.array_value {
        return Value::Array(array.values.into_iter().map(any_value_to_json).collect());
    }
    if let Some(kvlist) = value.kvlist_value {
        return attributes_to_json(kvlist.values);
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"{
      "resourceSpans": [{
        "resource": { "attributes": [
          { "key": "service.name", "value": { "stringValue": "checkout" } },
          { "key": "deployment.environment", "value": { "stringValue": "prod" } }
        ]},
        "scopeSpans": [{
          "scope": { "name": "example.http" },
          "spans": [
            {
              "traceId": "5B8EFFF798038103D269B633813FC60C",
              "spanId": "EEE19B7EC3C1B174",
              "parentSpanId": "",
              "name": "GET /cart",
              "kind": 2,
              "startTimeUnixNano": "1700000000000000000",
              "endTimeUnixNano": "1700000000250000000",
              "attributes": [
                { "key": "http.status_code", "value": { "intValue": "200" } },
                { "key": "http.route", "value": { "stringValue": "/cart" } },
                { "key": "tags", "value": { "arrayValue": { "values": [ { "stringValue": "a" }, { "boolValue": true } ] } } }
              ],
              "status": { "code": "STATUS_CODE_OK" }
            },
            {
              "traceId": "5b8efff798038103d269b633813fc60c",
              "spanId": "0f3d1f1cc9f0a8a1",
              "parentSpanId": "eee19b7ec3c1b174",
              "name": "SELECT cart",
              "kind": "SPAN_KIND_CLIENT",
              "startTimeUnixNano": 1700000000010000000,
              "endTimeUnixNano": 1700000000020000000,
              "status": { "code": 2, "message": "timeout" }
            },
            {
              "traceId": "not-hex",
              "spanId": "0f3d1f1cc9f0a8a1",
              "name": "broken",
              "startTimeUnixNano": "1",
              "endTimeUnixNano": "2"
            }
          ]
        }]
      }]
    }"#;

    #[test]
    fn flattens_sample_request() {
        let request: ExportTraceServiceRequest =
            serde_json::from_str(SAMPLE).expect("parse sample");
        let flat = flatten(request);

        assert_eq!(
            flat.rejected, 1,
            "the span with a non-hex trace id is rejected"
        );
        assert_eq!(flat.rows.len(), 2);

        let root = &flat.rows[0];
        assert_eq!(root.trace_id, "5b8efff798038103d269b633813fc60c");
        assert_eq!(root.span_id, "eee19b7ec3c1b174");
        assert_eq!(root.parent_span_id, None);
        assert_eq!(root.service_name, "checkout");
        assert_eq!(root.scope_name.as_deref(), Some("example.http"));
        assert_eq!(root.kind, 2);
        assert_eq!(root.status_code, 1);
        assert_eq!(root.end_ns - root.start_ns, 250_000_000);

        let attrs: serde_json::Value =
            serde_json::from_str(&root.attributes_json).expect("attrs json");
        assert_eq!(attrs["http.status_code"], 200);
        assert_eq!(attrs["http.route"], "/cart");
        assert_eq!(attrs["tags"], serde_json::json!(["a", true]));

        let child = &flat.rows[1];
        assert_eq!(child.parent_span_id.as_deref(), Some("eee19b7ec3c1b174"));
        assert_eq!(child.kind, 3, "enum name resolves to its proto number");
        assert_eq!(child.status_code, 2);
        assert_eq!(child.status_message.as_deref(), Some("timeout"));
        assert_eq!(child.attributes_json, "{}");
    }

    #[test]
    fn missing_service_name_falls_back() {
        let request: ExportTraceServiceRequest = serde_json::from_str(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":"0102030405060708090a0b0c0d0e0f10","spanId":"0102030405060708","startTimeUnixNano":"5","endTimeUnixNano":"6"}]}]}]}"#,
        )
        .expect("parse");
        let flat = flatten(request);
        assert_eq!(flat.rows[0].service_name, UNKNOWN_SERVICE);
    }

    #[test]
    fn rejects_zero_ids_and_inverted_timestamps() {
        assert_eq!(normalize_id("00000000000000000000000000000000", 32), None);
        assert_eq!(
            normalize_id("ABCDEF0123456789", 16).as_deref(),
            Some("abcdef0123456789")
        );
        assert_eq!(normalize_id("abcdef012345678", 16), None);

        let request: ExportTraceServiceRequest = serde_json::from_str(
            r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{"traceId":"0102030405060708090a0b0c0d0e0f10","spanId":"0102030405060708","startTimeUnixNano":"10","endTimeUnixNano":"5"}]}]}]}"#,
        )
        .expect("parse");
        let flat = flatten(request);
        assert!(flat.rows.is_empty());
        assert_eq!(flat.rejected, 1);
    }

    #[test]
    fn empty_request_is_fine() {
        let flat = flatten(ExportTraceServiceRequest::default());
        assert_eq!(flat, Flattened::default());
    }
}
