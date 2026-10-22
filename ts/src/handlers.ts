// HTTP handler for the plugin. Two caller kinds reach it:
//  * console users, verified by the SDK runtime → `extractAuthContext(req)`;
//  * OTLP exporters on the public `/v1/*` prefix → `X-Otel-Dash-Token` header.
// The Temps proxy strips Authorization/Cookie/x-temps-* before forwarding, so
// a bearer token would never arrive.

import type { IncomingMessage, ServerResponse } from "node:http";
import { extractAuthContext, secureSecretMatches, type PluginContext, type RequestHandler } from "@temps-sdk/plugin";
import { tokenPreview, type SpanStore } from "./db";
import type { IngestQueue } from "./ingest";
import { flatten } from "./otlp";
import type { PluginSettings, SettingsView, StatsResponse, UpdateSettings } from "./types";

export const INGEST_TOKEN_HEADER = "x-otel-dash-token";
export const MAX_INGEST_BODY_BYTES = 16 * 1024 * 1024;

export interface AppState {
  store: SpanStore;
  queue: IngestQueue;
  settings: PluginSettings;
  ingestToken: string;
  mountUrl: string | undefined;
}

export function createAppState(store: SpanStore, queue: IngestQueue, ctx: Pick<PluginContext, "mountUrl">): AppState {
  return { store, queue, settings: store.getSettings(), ingestToken: store.ingestToken(), mountUrl: ctx.mountUrl };
}

class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly retryAfter = false,
  ) {
    super(message);
  }
}

/** Collect the body. Must be called synchronously at handler entry: the SDK
 *  bridge emits `data`/`end` in a microtask after the handler is invoked. */
function readBody(req: IncomingMessage, limit = MAX_INGEST_BODY_BYTES): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > limit) reject(new HttpError(413, `Request body exceeds ${limit} bytes`));
      else chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
  });
}

function send(res: ServerResponse, status: number, body: unknown, extra: Record<string, string> = {}): void {
  res.writeHead(status, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store", ...extra });
  res.end(JSON.stringify(body));
}

function sendError(res: ServerResponse, error: unknown): void {
  if (error instanceof HttpError) {
    send(res, error.status, { error: error.message }, error.retryAfter ? { "Retry-After": "1" } : {});
    return;
  }
  console.error(JSON.stringify({ level: "error", message: "request failed", reason: error instanceof Error ? error.message : String(error) }));
  send(res, 500, { error: "Internal error" });
}

function header(req: IncomingMessage, name: string): string {
  const value = req.headers[name];
  return (Array.isArray(value) ? value[0] : value) ?? "";
}

function requireAdmin(req: IncomingMessage): void {
  const caller = extractAuthContext(req);
  if (!caller) throw new HttpError(401, "This route requires a caller authenticated by Temps");
  if (caller.role !== "admin" && caller.role !== "platform_admin") throw new HttpError(403, "This action requires an administrator");
}

function parseJson<T>(body: Buffer): T {
  try {
    return JSON.parse(body.toString("utf8")) as T;
  } catch (error) {
    throw new HttpError(400, `invalid JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function settingsView(state: AppState): SettingsView {
  return {
    ...state.settings,
    ingestUrl: state.mountUrl ? `${state.mountUrl}/v1/traces` : null,
    tokenPreview: tokenPreview(state.ingestToken),
  };
}

function nowNs(): bigint {
  return BigInt(Date.now()) * 1_000_000n;
}

export function createHandler(state: AppState): RequestHandler {
  return async (req, res) => {
    const body = readBody(req); // attach listeners before any await
    body.catch(() => undefined); // surfaced below where awaited
    const url = new URL(req.url ?? "/", "http://plugin");
    const method = req.method ?? "GET";
    const path = url.pathname;

    try {
      if (method === "POST" && path === "/v1/traces") return await ingestTraces(state, req, res, await body);
      if (method === "GET" && path === "/stats") return send(res, 200, stats(state));
      if (method === "GET" && path === "/spans") {
        return send(res, 200, state.store.recentSpans({
          limit: url.searchParams.has("limit") ? Number(url.searchParams.get("limit")) : undefined,
          service: url.searchParams.get("service") ?? undefined,
          errorsOnly: url.searchParams.get("errorsOnly") === "true",
        }));
      }
      const trace = path.match(/^\/traces\/([^/]+)$/);
      if (method === "GET" && trace) {
        const traceId = decodeURIComponent(trace[1] ?? "").toLowerCase();
        if (!/^[0-9a-f]{32}$/.test(traceId)) throw new HttpError(400, "trace id must be 32 hex characters");
        const detail = state.store.trace(traceId);
        if (!detail) throw new HttpError(404, "Not found");
        return send(res, 200, detail);
      }
      if (method === "GET" && path === "/services") return send(res, 200, state.store.services());
      if (method === "GET" && path === "/settings") return send(res, 200, settingsView(state));
      if (method === "PATCH" && path === "/settings") {
        requireAdmin(req);
        const update = parseJson<UpdateSettings>(await body);
        if (update.ingestEnabled !== undefined && typeof update.ingestEnabled !== "boolean") throw new HttpError(400, "ingestEnabled must be a boolean");
        if (update.retentionDays !== undefined && !Number.isFinite(update.retentionDays)) throw new HttpError(400, "retentionDays must be a number");
        state.settings = state.store.updateSettings(update);
        console.error(JSON.stringify({ level: "info", message: "settings updated", settings: state.settings }));
        return send(res, 200, settingsView(state));
      }
      if (method === "GET" && path === "/settings/token") {
        requireAdmin(req);
        return send(res, 200, { token: state.ingestToken });
      }
      if (method === "POST" && path === "/settings/token") {
        requireAdmin(req);
        state.ingestToken = state.store.rotateIngestToken();
        console.error(JSON.stringify({ level: "warn", message: "ingest token rotated" }));
        return send(res, 200, { token: state.ingestToken });
      }
      throw new HttpError(404, "Not found");
    } catch (error) {
      sendError(res, error);
    }
  };
}

async function ingestTraces(state: AppState, req: IncomingMessage, res: ServerResponse, body: Buffer): Promise<void> {
  if (!state.settings.ingestEnabled) throw new HttpError(503, "Ingest is disabled in the plugin settings", true);
  const provided = header(req, INGEST_TOKEN_HEADER).trim();
  if (!provided || !secureSecretMatches(provided, state.ingestToken)) throw new HttpError(401, "Missing or invalid ingest token");
  const contentType = header(req, "content-type");
  if (!contentType.startsWith("application/json")) {
    throw new HttpError(415, `Unsupported content type: ${contentType}. Send OTLP/JSON (application/json).`);
  }
  const flat = flatten(parseJson<unknown>(body));
  if (!state.queue.enqueue(flat.rows)) throw new HttpError(503, "ingest queue is full", true);
  send(res, 200, flat.rejected > 0
    ? { partialSuccess: { rejectedSpans: flat.rejected, errorMessage: "spans with invalid ids or timestamps were dropped" } }
    : {});
}

function stats(state: AppState): StatsResponse {
  const counts = state.store.counts(nowNs());
  return { ...counts, ...state.queue.stats() };
}
