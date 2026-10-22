// Batched write queue between the OTLP handler and SQLite. Handlers push flat
// rows and answer immediately; a timer-driven flush commits them in one
// transaction. A full queue is reported to the caller (503 → exporter retries).

import type { SpanStore } from "./db";
import type { SpanRow } from "./otlp";

export const DEFAULT_QUEUE_CAPACITY = 1024;
export const DEFAULT_FLUSH_INTERVAL_MS = 200;
export const MAX_BATCH_SPANS = 4000;

export interface QueueStats {
  acceptedTotal: number;
  droppedTotal: number;
  failedTotal: number;
  queueDepth: number;
  queueCapacity: number;
  lastIngestMs: number | null;
}

export class IngestQueue {
  readonly #store: SpanStore;
  readonly #capacity: number;
  readonly #flushIntervalMs: number;
  #pending: SpanRow[][] = [];
  #pendingSpans = 0;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #accepted = 0;
  #dropped = 0;
  #failed = 0;
  #lastIngestMs: number | null = null;
  #closed = false;

  constructor(store: SpanStore, capacity = DEFAULT_QUEUE_CAPACITY, flushIntervalMs = DEFAULT_FLUSH_INTERVAL_MS) {
    this.#store = store;
    this.#capacity = capacity;
    this.#flushIntervalMs = flushIntervalMs;
  }

  /** Accept a batch without touching the database. Returns false when full. */
  enqueue(rows: SpanRow[]): boolean {
    if (rows.length === 0) return true;
    if (this.#closed || this.#pending.length >= this.#capacity) {
      this.#dropped += rows.length;
      return false;
    }
    this.#pending.push(rows);
    this.#pendingSpans += rows.length;
    this.#accepted += rows.length;
    this.#lastIngestMs = Date.now();
    if (this.#pendingSpans >= MAX_BATCH_SPANS) {
      this.flush();
    } else if (this.#timer === null) {
      this.#timer = setTimeout(() => this.flush(), this.#flushIntervalMs);
      this.#timer.unref?.();
    }
    return true;
  }

  /** Commit everything pending now. */
  flush(): void {
    if (this.#timer !== null) {
      clearTimeout(this.#timer);
      this.#timer = null;
    }
    if (this.#pending.length === 0) return;
    const batch = this.#pending.flat();
    this.#pending = [];
    this.#pendingSpans = 0;
    const started = performance.now();
    try {
      const written = this.#store.insertSpans(batch);
      console.error(JSON.stringify({ level: "debug", message: "flushed span batch", spans: batch.length, written, elapsed_ms: Math.round(performance.now() - started) }));
    } catch (error) {
      this.#failed += batch.length;
      console.error(JSON.stringify({ level: "error", message: "failed to persist span batch", spans: batch.length, reason: error instanceof Error ? error.message : String(error) }));
    }
  }

  close(): void {
    this.#closed = true;
    this.flush();
  }

  stats(): QueueStats {
    return {
      acceptedTotal: this.#accepted,
      droppedTotal: this.#dropped,
      failedTotal: this.#failed,
      queueDepth: this.#pending.length,
      queueCapacity: this.#capacity,
      lastIngestMs: this.#lastIngestMs,
    };
  }
}
