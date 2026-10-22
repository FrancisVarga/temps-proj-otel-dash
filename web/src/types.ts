// Mirrors src/types.rs (serde camelCase).

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

export interface IngestTokenReveal {
  token: string;
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
