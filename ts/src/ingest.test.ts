import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SpanStore } from "./db";
import { IngestQueue } from "./ingest";
import { row } from "./db.test";

const dirs: string[] = [];
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });

function open(): SpanStore {
  const dir = mkdtempSync(join(tmpdir(), "otel-dash-q-"));
  dirs.push(dir);
  return SpanStore.open(dir);
}

test("batches flush on the timer and are counted", async () => {
  const store = open();
  const queue = new IngestQueue(store, 16, 20);
  expect(queue.enqueue([row("t1", "a", "svc", 1, 1, 0), row("t2", "b", "svc", 2, 1, 0)])).toBe(true);
  expect(queue.enqueue([row("t3", "c", "svc", 3, 1, 0)])).toBe(true);
  expect(queue.enqueue([])).toBe(true);
  expect(queue.stats()).toMatchObject({ acceptedTotal: 3, queueDepth: 2, queueCapacity: 16 });
  expect(store.counts(0n).totalSpans).toBe(0);
  await Bun.sleep(60);
  expect(store.counts(0n).totalSpans).toBe(3);
  expect(queue.stats()).toMatchObject({ acceptedTotal: 3, droppedTotal: 0, failedTotal: 0, queueDepth: 0 });
  expect(queue.stats().lastIngestMs).not.toBeNull();
});

test("a full queue rejects without blocking and close flushes", () => {
  const store = open();
  const queue = new IngestQueue(store, 2, 60_000);
  expect(queue.enqueue([row("t1", "a", "svc", 1, 1, 0)])).toBe(true);
  expect(queue.enqueue([row("t2", "b", "svc", 2, 1, 0)])).toBe(true);
  expect(queue.enqueue([row("t3", "c", "svc", 3, 1, 0), row("t4", "d", "svc", 4, 1, 0)])).toBe(false);
  expect(queue.stats()).toMatchObject({ acceptedTotal: 2, droppedTotal: 2, queueDepth: 2 });
  queue.close();
  expect(store.counts(0n).totalSpans).toBe(2);
  expect(queue.enqueue([row("t5", "e", "svc", 5, 1, 0)])).toBe(false);
});
