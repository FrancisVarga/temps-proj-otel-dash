import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SpanStore } from "./db";
import { createAppState, createHandler, INGEST_TOKEN_HEADER, type AppState } from "./handlers";
import { IngestQueue } from "./ingest";

const TRACES = JSON.stringify({ resourceSpans: [{ resource: { attributes: [{ key: "service.name", value: { stringValue: "web" } }] }, scopeSpans: [{ spans: [
  { traceId: "5b8efff798038103d269b633813fc60c", spanId: "eee19b7ec3c1b174", name: "GET /", kind: 2, startTimeUnixNano: "1700000000000000000", endTimeUnixNano: "1700000000100000000" },
  { traceId: "bad", spanId: "eee19b7ec3c1b174", name: "x", startTimeUnixNano: "1", endTimeUnixNano: "2" },
] }] }] });

const dirs: string[] = [];
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });

function app(): { handle: ReturnType<typeof createHandler>; state: AppState } {
  const dir = mkdtempSync(join(tmpdir(), "otel-dash-h-"));
  dirs.push(dir);
  const store = SpanStore.open(dir);
  const queue = new IngestQueue(store, 8, 10);
  const state = createAppState(store, queue, { mountUrl: "http://temps.local/api/x/otel-dash" });
  return { handle: createHandler(state), state };
}

interface Reply { status: number; headers: Record<string, string>; json: unknown }

/** Mimics the SDK bridge: body is emitted in a microtask after the handler runs. */
function request(handle: ReturnType<typeof createHandler>, method: string, url: string, headers: Record<string, string> = {}, body = ""): Promise<Reply> {
  return new Promise((resolve) => {
    const data: ((chunk: Buffer) => void)[] = [];
    const end: (() => void)[] = [];
    const req = {
      url, method, headers,
      on(event: string, listener: (...args: never[]) => void) {
        if (event === "data") data.push(listener as (chunk: Buffer) => void);
        if (event === "end") end.push(listener as () => void);
        return req;
      },
    } as unknown as IncomingMessage;
    let status = 200;
    let outHeaders: Record<string, string> = {};
    const res = {
      writeHead(code: number, values?: Record<string, string>) { status = code; outHeaders = { ...outHeaders, ...values }; return res; },
      end(value?: string) { let json: unknown = null; try { json = value ? JSON.parse(value) : null; } catch { json = value; } resolve({ status, headers: outHeaders, json }); return res; },
    } as unknown as ServerResponse;
    void handle(req, res);
    queueMicrotask(() => {
      if (body) for (const l of data) l(Buffer.from(body));
      for (const l of end) l();
    });
  });
}

const json = { "content-type": "application/json" };

describe("ingest", () => {
  test("requires the token header and persists spans", async () => {
    const { handle, state } = app();
    const token = state.ingestToken;
    expect((await request(handle, "POST", "/v1/traces", json, TRACES)).status).toBe(401);
    expect((await request(handle, "POST", "/v1/traces", { ...json, [INGEST_TOKEN_HEADER]: "otd_wrong" }, TRACES)).status).toBe(401);
    expect((await request(handle, "POST", "/v1/traces", { ...json, authorization: `Bearer ${token}` }, TRACES)).status).toBe(401);

    const ok = await request(handle, "POST", "/v1/traces", { ...json, [INGEST_TOKEN_HEADER]: token }, TRACES);
    expect(ok.status).toBe(200);
    expect(ok.json).toEqual({ partialSuccess: { rejectedSpans: 1, errorMessage: "spans with invalid ids or timestamps were dropped" } });
    await Bun.sleep(40);

    const stats = await request(handle, "GET", "/stats");
    expect(stats.json).toMatchObject({ totalSpans: 1, acceptedTotal: 1, queueCapacity: 8 });
    const spans = await request(handle, "GET", "/spans?service=web");
    expect((spans.json as { name: string; durationMs: number }[])[0]).toMatchObject({ name: "GET /", durationMs: 100 });
    const trace = await request(handle, "GET", "/traces/5B8EFFF798038103D269B633813FC60C");
    expect(trace.status).toBe(200);
    expect((trace.json as { spans: unknown[] }).spans).toHaveLength(1);
    expect((await request(handle, "GET", "/traces/nope")).status).toBe(400);
    expect((await request(handle, "GET", "/traces/00000000000000000000000000000001")).status).toBe(404);
    const services = await request(handle, "GET", "/services");
    expect((services.json as { serviceName: string }[])[0]!.serviceName).toBe("web");
  });

  test("rejects bad payloads and reports disabled ingest with Retry-After", async () => {
    const { handle, state } = app();
    const auth = { [INGEST_TOKEN_HEADER]: state.ingestToken };
    const proto = await request(handle, "POST", "/v1/traces", { ...auth, "content-type": "application/x-protobuf" }, "x");
    expect(proto.status).toBe(415);
    expect((await request(handle, "POST", "/v1/traces", { ...auth, ...json }, "{not json")).status).toBe(400);
    state.settings = { ...state.settings, ingestEnabled: false };
    const off = await request(handle, "POST", "/v1/traces", { ...auth, ...json }, TRACES);
    expect(off.status).toBe(503);
    expect(off.headers["Retry-After"]).toBe("1");
  });
});

describe("settings", () => {
  test("readable by anyone, mutations need a verified caller", async () => {
    const { handle } = app();
    const view = await request(handle, "GET", "/settings");
    expect(view.json).toMatchObject({ ingestEnabled: true, retentionDays: 7, ingestUrl: "http://temps.local/api/x/otel-dash/v1/traces" });
    expect((view.json as { tokenPreview: string }).tokenPreview).toStartWith("otd_");
    expect(view.json as object).not.toHaveProperty("token");
    expect((await request(handle, "PATCH", "/settings", json, '{"retentionDays":3}')).status).toBe(401);
    expect((await request(handle, "GET", "/settings/token")).status).toBe(401);
    expect((await request(handle, "POST", "/settings/token")).status).toBe(401);
    expect((await request(handle, "GET", "/nope")).status).toBe(404);
  });
});
