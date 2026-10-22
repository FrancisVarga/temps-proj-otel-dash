//! Shared API types for the OpenTelemetry Dashboard plugin.
//!
//! Everything here crosses the HTTP boundary, so every struct is `camelCase`
//! on the wire to match the React UI.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// Settings
// ============================================================================

/// Plugin-level configuration persisted in SQLite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginSettings {
    /// Accept OTLP payloads on `/v1/*`. Turning this off answers 503 to exporters.
    pub ingest_enabled: bool,
    /// Spans older than this are pruned by the retention task.
    pub retention_days: u32,
}

impl PluginSettings {
    pub const DEFAULT_INGEST_ENABLED: bool = true;
    pub const DEFAULT_RETENTION_DAYS: u32 = 7;
    pub const MAX_RETENTION_DAYS: u32 = 365;
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            ingest_enabled: Self::DEFAULT_INGEST_ENABLED,
            retention_days: Self::DEFAULT_RETENTION_DAYS,
        }
    }
}

/// Partial update for plugin settings — only `Some` fields are written.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

/// Settings as shown in the UI: persisted values plus derived ingest details.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    #[serde(flatten)]
    pub settings: PluginSettings,
    /// Absolute OTLP/HTTP traces endpoint when Temps told us its public API URL.
    pub ingest_url: Option<String>,
    /// First characters of the ingest token so an operator can recognise it.
    pub token_preview: String,
}

/// Full ingest token — admin only.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngestTokenReveal {
    pub token: String,
}

// ============================================================================
// Ingest
// ============================================================================

/// OTLP/HTTP `ExportTraceServiceResponse`. Exporters understand this shape.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportTraceResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_success: Option<PartialSuccess>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PartialSuccess {
    pub rejected_spans: u64,
    pub error_message: String,
}

// ============================================================================
// Query
// ============================================================================

/// A stored span, flattened for the dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpanSummary {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub service_name: String,
    pub scope_name: Option<String>,
    pub name: String,
    /// OTLP `SpanKind` (0 unspecified, 1 internal, 2 server, 3 client, 4 producer, 5 consumer).
    pub kind: i32,
    /// Start time in Unix milliseconds (safe for JavaScript numbers).
    pub start_ms: i64,
    pub duration_ms: f64,
    /// OTLP `StatusCode` (0 unset, 1 ok, 2 error).
    pub status_code: i32,
    pub status_message: Option<String>,
    pub attributes: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceDetail {
    pub trace_id: String,
    pub spans: Vec<SpanSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSummary {
    pub service_name: String,
    pub span_count: i64,
    pub error_count: i64,
    pub avg_duration_ms: f64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatsResponse {
    pub total_spans: i64,
    pub total_traces: i64,
    pub total_services: i64,
    pub spans_last_hour: i64,
    /// Spans accepted into the write queue since the process started.
    pub accepted_total: u64,
    /// Spans refused because the queue was full since the process started.
    pub dropped_total: u64,
    /// Spans that failed to persist since the process started.
    pub failed_total: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    /// Unix milliseconds of the last accepted batch, if any.
    pub last_ingest_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct SpanQuery {
    /// Maximum rows to return (1..=500, default 100).
    pub limit: Option<u64>,
    /// Restrict to one `service.name`.
    pub service: Option<String>,
    /// Only spans with an error status.
    pub errors_only: Option<bool>,
}

impl SpanQuery {
    pub const DEFAULT_LIMIT: u64 = 100;
    pub const MAX_LIMIT: u64 = 500;

    pub fn effective_limit(&self) -> u64 {
        self.limit
            .unwrap_or(Self::DEFAULT_LIMIT)
            .clamp(1, Self::MAX_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_serialize_as_camel_case() {
        let json = serde_json::to_value(PluginSettings::default()).expect("serialize");
        assert_eq!(json["ingestEnabled"], true);
        assert_eq!(json["retentionDays"], 7);
    }

    #[test]
    fn span_query_limit_is_clamped() {
        assert_eq!(SpanQuery::default().effective_limit(), 100);
        assert_eq!(
            SpanQuery {
                limit: Some(0),
                ..Default::default()
            }
            .effective_limit(),
            1
        );
        assert_eq!(
            SpanQuery {
                limit: Some(9999),
                ..Default::default()
            }
            .effective_limit(),
            500
        );
    }
}
