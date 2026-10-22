// Minimal OTLP/JSON trace decoding into flat rows. Field names follow the OTLP
// JSON mapping; ids are hex; enums may be numbers or proto names.

export const UNKNOWN_SERVICE = "unknown_service";
const SERVICE_NAME_ATTR = "service.name";
const TRACE_ID_LEN = 32;
const SPAN_ID_LEN = 16;
const SPAN_KINDS = ["SPAN_KIND_UNSPECIFIED", "SPAN_KIND_INTERNAL", "SPAN_KIND_SERVER", "SPAN_KIND_CLIENT", "SPAN_KIND_PRODUCER", "SPAN_KIND_CONSUMER"];
const STATUS_CODES = ["STATUS_CODE_UNSET", "STATUS_CODE_OK", "STATUS_CODE_ERROR"];

export interface SpanRow {
  traceId: string;
  spanId: string;
  parentSpanId: string | null;
  serviceName: string;
  scopeName: string | null;
  name: string;
  kind: number;
  /** Nanoseconds since the Unix epoch. Fits in 64 bits; stored as BigInt. */
  startNs: bigint;
  endNs: bigint;
  statusCode: number;
  statusMessage: string | null;
  attributesJson: string;
}

export interface Flattened {
  rows: SpanRow[];
  rejected: number;
}

type Json = Record<string, unknown>;

function obj(value: unknown): Json | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Json) : null;
}

function arr(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function enumValue(value: unknown, names: string[]): number {
  if (typeof value === "number" && Number.isInteger(value)) return value;
  if (typeof value === "string") {
    const index = names.findIndex((name) => name.toLowerCase() === value.toLowerCase());
    return index >= 0 ? index : 0;
  }
  return 0;
}

function nanos(value: unknown): bigint | null {
  try {
    if (typeof value === "number" && Number.isFinite(value) && value >= 0) return BigInt(Math.trunc(value));
    if (typeof value === "string" && /^\d+$/.test(value)) {
      const n = BigInt(value);
      return n <= 0x7fff_ffff_ffff_ffffn ? n : null;
    }
  } catch {
    return null;
  }
  return null;
}

function normalizeId(raw: unknown, len: number): string | null {
  if (typeof raw !== "string" || raw.length !== len || !/^[0-9a-fA-F]+$/.test(raw)) return null;
  if (/^0+$/.test(raw)) return null;
  return raw.toLowerCase();
}

export function anyValueToJson(value: unknown): unknown {
  const v = obj(value);
  if (!v) return null;
  if (typeof v.stringValue === "string") return v.stringValue;
  if (typeof v.boolValue === "boolean") return v.boolValue;
  if (v.intValue !== undefined) {
    const n = typeof v.intValue === "number" ? v.intValue : Number(v.intValue);
    return Number.isFinite(n) ? n : null;
  }
  if (typeof v.doubleValue === "number") return Number.isFinite(v.doubleValue) ? v.doubleValue : null;
  if (typeof v.bytesValue === "string") return v.bytesValue;
  const array = obj(v.arrayValue);
  if (array) return arr(array.values).map(anyValueToJson);
  const kvlist = obj(v.kvlistValue);
  if (kvlist) return attributesToJson(arr(kvlist.values));
  return null;
}

export function attributesToJson(attributes: unknown[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const item of attributes) {
    const kv = obj(item);
    const key = kv ? str(kv.key) : "";
    if (!kv || !key) continue;
    out[key] = kv.value === undefined ? null : anyValueToJson(kv.value);
  }
  return out;
}

function serviceNameOf(resource: Json | null): string {
  if (!resource) return UNKNOWN_SERVICE;
  for (const item of arr(resource.attributes)) {
    const kv = obj(item);
    if (kv && kv.key === SERVICE_NAME_ATTR) {
      const name = str(obj(kv.value)?.stringValue);
      return name || UNKNOWN_SERVICE;
    }
  }
  return UNKNOWN_SERVICE;
}

function flattenSpan(span: Json, serviceName: string, scopeName: string | null): SpanRow | null {
  const traceId = normalizeId(span.traceId, TRACE_ID_LEN);
  const spanId = normalizeId(span.spanId, SPAN_ID_LEN);
  if (!traceId || !spanId) return null;
  const startNs = nanos(span.startTimeUnixNano);
  const endNs = nanos(span.endTimeUnixNano);
  if (startNs === null || endNs === null || endNs < startNs) return null;

  const status = obj(span.status);
  const statusMessage = status ? str(status.message) : "";
  return {
    traceId,
    spanId,
    parentSpanId: normalizeId(span.parentSpanId, SPAN_ID_LEN),
    serviceName,
    scopeName,
    name: str(span.name),
    kind: enumValue(span.kind, SPAN_KINDS),
    startNs,
    endNs,
    statusCode: status ? enumValue(status.code, STATUS_CODES) : 0,
    statusMessage: statusMessage || null,
    attributesJson: JSON.stringify(attributesToJson(arr(span.attributes))),
  };
}

/** Flatten an `ExportTraceServiceRequest`; unusable spans are counted, not fatal. */
export function flatten(request: unknown): Flattened {
  const out: Flattened = { rows: [], rejected: 0 };
  const root = obj(request);
  if (!root) return out;
  for (const rs of arr(root.resourceSpans)) {
    const resourceSpans = obj(rs);
    if (!resourceSpans) continue;
    const serviceName = serviceNameOf(obj(resourceSpans.resource));
    for (const ss of arr(resourceSpans.scopeSpans)) {
      const scopeSpans = obj(ss);
      if (!scopeSpans) continue;
      const scopeName = str(obj(scopeSpans.scope)?.name) || null;
      for (const s of arr(scopeSpans.spans)) {
        const span = obj(s);
        const row = span ? flattenSpan(span, serviceName, scopeName) : null;
        if (row) out.rows.push(row);
        else out.rejected += 1;
      }
    }
  }
  return out;
}
