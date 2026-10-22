import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SpanStore, tokenPreview } from "./db";
import type { SpanRow } from "./otlp";
import { DEFAULT_SETTINGS, MAX_RETENTION_DAYS } from "./types";

const MS = 1_000_000n;
const dirs: string[] = [];
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });

function open(): { store: SpanStore; dir: string } {
  const dir = mkdtempSync(join(tmpdir(), "otel-dash-"));
  dirs.push(dir);
  return { store: SpanStore.open(dir), dir };
}

export function row(trace: string, span: string, service: string, startMs: number, durationMs: number, status: number): SpanRow {
  const startNs = BigInt(startMs) * MS;
  return { traceId: trace, spanId: span, parentSpanId: null, serviceName: service, scopeName: null, name: `op-${span}`, kind: 2, startNs, endNs: startNs + BigInt(durationMs) * MS, statusCode: status, statusMessage: null, attributesJson: '{"k":"v"}' };
}

describe("SpanStore", () => {
  test("insert is idempotent and queries work", () => {
    const { store } = open();
    const rows = [row("t1", "s1", "api", 1000, 50, 1), row("t1", "s2", "db", 1010, 10, 2), row("t2", "s3", "api", 2000, 100, 0)];
    expect(store.insertSpans(rows)).toBe(3);
    expect(store.insertSpans(rows)).toBe(0);

    const recent = store.recentSpans({});
    expect(recent.map((s) => s.spanId)).toEqual(["s3", "s2", "s1"]);
    expect(recent[0]).toMatchObject({ startMs: 2000, durationMs: 100, attributes: { k: "v" } });
    expect(store.recentSpans({ service: "api" })).toHaveLength(2);
    expect(store.recentSpans({ errorsOnly: true }).map((s) => s.spanId)).toEqual(["s2"]);
    expect(store.recentSpans({ limit: 1 })).toHaveLength(1);

    expect(store.trace("t1")!.spans.map((s) => s.spanId)).toEqual(["s1", "s2"]);
    expect(store.trace("missing")).toBeNull();

    const services = store.services();
    expect(services[0]).toEqual({ serviceName: "api", spanCount: 2, errorCount: 0, avgDurationMs: 75 });
    expect(services[1]!.errorCount).toBe(1);

    expect(store.counts(2500n * MS)).toEqual({ totalSpans: 3, totalTraces: 2, totalServices: 2, spansLastHour: 3 });
  });

  test("large batches commit in one transaction", () => {
    const { store } = open();
    const rows = Array.from({ length: 4017 }, (_, i) => row(`t${i}`, "s", "svc", i, 1, 0));
    expect(store.insertSpans(rows)).toBe(4017);
    expect(store.counts(0n).totalSpans).toBe(4017);
  });

  test("prune removes old spans", () => {
    const { store } = open();
    store.insertSpans([row("t1", "old", "svc", 10, 1, 0), row("t2", "new", "svc", 100, 1, 0)]);
    expect(store.pruneBefore(50n * MS)).toBe(1);
    expect(store.recentSpans({}).map((s) => s.spanId)).toEqual(["new"]);
  });

  test("settings default, update and clamp", () => {
    const { store } = open();
    expect(store.getSettings()).toEqual(DEFAULT_SETTINGS);
    const updated = store.updateSettings({ ingestEnabled: false, retentionDays: 9999 });
    expect(updated).toEqual({ ingestEnabled: false, retentionDays: MAX_RETENTION_DAYS });
    expect(store.getSettings()).toEqual(updated);
    expect(store.updateSettings({ retentionDays: 0 })).toEqual({ ingestEnabled: false, retentionDays: 1 });
  });

  test("ingest token is created once, survives reopen and rotates", () => {
    const { store, dir } = open();
    const first = store.ingestToken();
    expect(first).toMatch(/^otd_[0-9a-f]{64}$/);
    expect(store.ingestToken()).toBe(first);
    store.close();
    const reopened = SpanStore.open(dir);
    expect(reopened.ingestToken()).toBe(first);
    const rotated = reopened.rotateIngestToken();
    expect(rotated).not.toBe(first);
    expect(reopened.ingestToken()).toBe(rotated);
    expect(tokenPreview(rotated)).toBe(`${rotated.slice(0, 10)}…`);
  });
});
