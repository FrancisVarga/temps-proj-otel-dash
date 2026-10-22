import type {
  IngestTokenReveal,
  ServiceSummary,
  SettingsView,
  SpanQuery,
  SpanSummary,
  StatsResponse,
  TraceDetail,
  UpdateSettings,
} from "./types";

// Absolute: the UI is served from /api/x/otel-dash/ui/, the API one level up.
const API_BASE = "/api/x/otel-dash";

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    ...options,
    headers: { "Content-Type": "application/json", ...options?.headers },
  });
  if (!res.ok) {
    let message = res.statusText;
    try {
      const body = (await res.json()) as { error?: string };
      if (body.error) message = body.error;
    } catch {
      // Non-JSON error body: keep the status text.
    }
    throw new ApiError(res.status, message);
  }
  if (res.status === 204) return null as T;
  return (await res.json()) as T;
}

export const getStats = () => request<StatsResponse>("/stats");
export const getServices = () => request<ServiceSummary[]>("/services");
export const getTrace = (traceId: string) =>
  request<TraceDetail>(`/traces/${encodeURIComponent(traceId)}`);

export function getSpans(query: SpanQuery = {}): Promise<SpanSummary[]> {
  const params = new URLSearchParams();
  if (query.limit) params.set("limit", String(query.limit));
  if (query.service) params.set("service", query.service);
  if (query.errorsOnly) params.set("errorsOnly", "true");
  const suffix = params.size ? `?${params}` : "";
  return request(`/spans${suffix}`);
}

export const getSettings = () => request<SettingsView>("/settings");
export const updateSettings = (update: UpdateSettings) =>
  request<SettingsView>("/settings", { method: "PATCH", body: JSON.stringify(update) });
export const revealToken = () => request<IngestTokenReveal>("/settings/token");
export const rotateToken = () => request<IngestTokenReveal>("/settings/token", { method: "POST" });
