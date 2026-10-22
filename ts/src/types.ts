// Wire types (camelCase). Mirrors ../../src/types.rs and ../../web/src/types.ts.

export interface PluginSettings {
  ingestEnabled: boolean;
  retentionDays: number;
}

export interface UpdateSettings {
  ingestEnabled?: boolean;
  retentionDays?: number;
}

export interface SettingsView extends PluginSettings {
  ingestUrl: string | null;
  tokenPreview: string;
}

export interface SpanSummary {
  traceId: string;
  spanId: string;
  parentSpanId: string | null;
  serviceName: string;
  scopeName: string | null;
  name: string;
  kind: number;
  startMs: number;
  durationMs: number;
  statusCode: number;
  statusMessage: string | null;
  attributes: Record<string, unknown> | null;
}

export interface TraceDetail {
  traceId: string;
  spans: SpanSummary[];
}

export interface ServiceSummary {
  serviceName: string;
  spanCount: number;
  errorCount: number;
  avgDurationMs: number;
}

export interface StatsResponse {
  totalSpans: number;
  totalTraces: number;
  totalServices: number;
  spansLastHour: number;
  acceptedTotal: number;
  droppedTotal: number;
  failedTotal: number;
  queueDepth: number;
  queueCapacity: number;
  lastIngestMs: number | null;
}

export interface SpanQuery {
  limit?: number;
  service?: string;
  errorsOnly?: boolean;
}

export const DEFAULT_SETTINGS: PluginSettings = { ingestEnabled: true, retentionDays: 7 };
export const MAX_RETENTION_DAYS = 365;
export const DEFAULT_SPAN_LIMIT = 100;
export const MAX_SPAN_LIMIT = 500;

export function effectiveLimit(query: SpanQuery): number {
  const raw = query.limit ?? DEFAULT_SPAN_LIMIT;
  return Math.min(Math.max(Math.trunc(raw) || 1, 1), MAX_SPAN_LIMIT);
}
