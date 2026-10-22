// SQLite persistence on bun:sqlite. One synchronous connection: SQLite has a
// single writer anyway, and bun:sqlite calls are sub-millisecond, so a pool
// would add contention without adding throughput. WAL keeps reads cheap while
// the writer commits.

import { Database, type Statement } from "bun:sqlite";
import { join } from "node:path";
import type { SpanRow } from "./otlp";
import {
  DEFAULT_SETTINGS,
  MAX_RETENTION_DAYS,
  effectiveLimit,
  type PluginSettings,
  type ServiceSummary,
  type SpanQuery,
  type SpanSummary,
  type TraceDetail,
  type UpdateSettings,
} from "./types";

export const DB_FILE = "otel-dash.db";
const SETTINGS_KEY = "plugin_settings";
const INGEST_TOKEN_KEY = "ingest_token";
const INGEST_TOKEN_PREFIX = "otd_";
const NS_PER_MS = 1_000_000n;

export interface SpanCounts {
  totalSpans: number;
  totalTraces: number;
  totalServices: number;
  spansLastHour: number;
}

interface SpanModel {
  trace_id: string;
  span_id: string;
  parent_span_id: string | null;
  service_name: string;
  scope_name: string | null;
  name: string;
  kind: number;
  start_ns: bigint;
  end_ns: bigint;
  status_code: number;
  status_message: string | null;
  attributes_json: string;
}

const DDL = [
  `CREATE TABLE IF NOT EXISTS spans (
     trace_id        TEXT    NOT NULL,
     span_id         TEXT    NOT NULL,
     parent_span_id  TEXT,
     service_name    TEXT    NOT NULL,
     scope_name      TEXT,
     name            TEXT    NOT NULL,
     kind            INTEGER NOT NULL DEFAULT 0,
     start_ns        INTEGER NOT NULL,
     end_ns          INTEGER NOT NULL,
     status_code     INTEGER NOT NULL DEFAULT 0,
     status_message  TEXT,
     attributes_json TEXT    NOT NULL DEFAULT '{}',
     PRIMARY KEY (trace_id, span_id)
   ) WITHOUT ROWID`,
  "CREATE INDEX IF NOT EXISTS idx_spans_start ON spans (start_ns DESC)",
  "CREATE INDEX IF NOT EXISTS idx_spans_service_start ON spans (service_name, start_ns DESC)",
  "CREATE INDEX IF NOT EXISTS idx_spans_status_start ON spans (status_code, start_ns DESC)",
  "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL)",
];

export class SpanStore {
  readonly #db: Database;
  readonly #insert: Statement;
  readonly #insertMany: (rows: SpanRow[]) => number;

  private constructor(db: Database) {
    this.#db = db;
    this.#insert = db.prepare(
      `INSERT OR IGNORE INTO spans
         (trace_id, span_id, parent_span_id, service_name, scope_name, name, kind, start_ns, end_ns, status_code, status_message, attributes_json)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)`,
    );
    this.#insertMany = db.transaction((rows: SpanRow[]) => {
      let written = 0;
      for (const r of rows) {
        written += this.#insert.run(
          r.traceId, r.spanId, r.parentSpanId, r.serviceName, r.scopeName, r.name, r.kind,
          r.startNs, r.endNs, r.statusCode, r.statusMessage, r.attributesJson,
        ).changes;
      }
      return written;
    });
  }

  /** Open (or create) the database in `dataDir` and make sure a token exists. */
  static open(dataDir: string): SpanStore {
    const db = new Database(join(dataDir, DB_FILE), { create: true, safeIntegers: true });
    db.exec("PRAGMA journal_mode = WAL");
    db.exec("PRAGMA synchronous = NORMAL");
    db.exec("PRAGMA busy_timeout = 5000");
    db.exec("PRAGMA temp_store = memory");
    db.exec("PRAGMA cache_size = -32768");
    for (const sql of DDL) db.exec(sql);
    const store = new SpanStore(db);
    store.ingestToken();
    return store;
  }

  close(): void {
    this.#db.close();
  }

  // -- spans: write ---------------------------------------------------------

  /** Insert in one transaction, ignoring duplicates. Returns rows written. */
  insertSpans(rows: SpanRow[]): number {
    return rows.length === 0 ? 0 : this.#insertMany(rows);
  }

  pruneBefore(cutoffNs: bigint): number {
    return this.#db.prepare("DELETE FROM spans WHERE start_ns < ?1").run(cutoffNs).changes;
  }

  // -- spans: read ----------------------------------------------------------

  recentSpans(query: SpanQuery): SpanSummary[] {
    const where: string[] = [];
    const params: (string | number)[] = [];
    if (query.service) {
      where.push("service_name = ?");
      params.push(query.service);
    }
    if (query.errorsOnly) where.push("status_code = 2");
    const sql = `SELECT * FROM spans ${where.length ? `WHERE ${where.join(" AND ")}` : ""} ORDER BY start_ns DESC LIMIT ?`;
    params.push(effectiveLimit(query));
    return (this.#db.prepare(sql).all(...params) as SpanModel[]).map(toSummary);
  }

  trace(traceId: string): TraceDetail | null {
    const rows = this.#db
      .prepare("SELECT * FROM spans WHERE trace_id = ?1 ORDER BY start_ns ASC")
      .all(traceId) as SpanModel[];
    return rows.length ? { traceId, spans: rows.map(toSummary) } : null;
  }

  services(): ServiceSummary[] {
    const rows = this.#db
      .prepare(
        `SELECT service_name,
                COUNT(*)                              AS span_count,
                COALESCE(SUM(status_code = 2), 0)     AS error_count,
                COALESCE(AVG(end_ns - start_ns), 0.0) AS avg_duration_ns
         FROM spans GROUP BY service_name ORDER BY span_count DESC`,
      )
      .all() as { service_name: string; span_count: bigint; error_count: bigint; avg_duration_ns: number | bigint }[];
    return rows.map((r) => ({
      serviceName: r.service_name,
      spanCount: Number(r.span_count),
      errorCount: Number(r.error_count),
      avgDurationMs: Number(r.avg_duration_ns) / 1_000_000,
    }));
  }

  counts(nowNs: bigint): SpanCounts {
    const totals = this.#db
      .prepare(
        `SELECT COUNT(*) AS total_spans, COUNT(DISTINCT trace_id) AS total_traces,
                COUNT(DISTINCT service_name) AS total_services FROM spans`,
      )
      .get() as { total_spans: bigint; total_traces: bigint; total_services: bigint };
    const lastHour = this.#db
      .prepare("SELECT COUNT(*) AS n FROM spans WHERE start_ns >= ?1")
      .get(nowNs - 3_600n * 1_000_000_000n) as { n: bigint };
    return {
      totalSpans: Number(totals.total_spans),
      totalTraces: Number(totals.total_traces),
      totalServices: Number(totals.total_services),
      spansLastHour: Number(lastHour.n),
    };
  }

  // -- settings -------------------------------------------------------------

  getSettings(): PluginSettings {
    const raw = this.#getSetting(SETTINGS_KEY);
    if (raw === null) return { ...DEFAULT_SETTINGS };
    const parsed: unknown = JSON.parse(raw);
    const p = parsed as Partial<PluginSettings>;
    if (typeof p.ingestEnabled !== "boolean" || typeof p.retentionDays !== "number") {
      throw new Error("Stored settings are corrupt");
    }
    return { ingestEnabled: p.ingestEnabled, retentionDays: p.retentionDays };
  }

  updateSettings(update: UpdateSettings): PluginSettings {
    const current = this.getSettings();
    const next: PluginSettings = {
      ingestEnabled: update.ingestEnabled ?? current.ingestEnabled,
      retentionDays: Math.min(Math.max(Math.trunc(update.retentionDays ?? current.retentionDays), 1), MAX_RETENTION_DAYS),
    };
    this.#putSetting(SETTINGS_KEY, JSON.stringify(next));
    return next;
  }

  ingestToken(): string {
    return this.#getSetting(INGEST_TOKEN_KEY) ?? this.rotateIngestToken();
  }

  rotateIngestToken(): string {
    const token = generateToken();
    this.#putSetting(INGEST_TOKEN_KEY, token);
    return token;
  }

  #getSetting(key: string): string | null {
    const row = this.#db.prepare("SELECT value FROM settings WHERE key = ?1").get(key) as { value: string } | null;
    return row?.value ?? null;
  }

  #putSetting(key: string, value: string): void {
    this.#db
      .prepare("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
      .run(key, value);
  }
}

function toSummary(m: SpanModel): SpanSummary {
  let attributes: Record<string, unknown> | null = null;
  try {
    attributes = JSON.parse(m.attributes_json) as Record<string, unknown>;
  } catch {
    attributes = null;
  }
  return {
    traceId: m.trace_id,
    spanId: m.span_id,
    parentSpanId: m.parent_span_id,
    serviceName: m.service_name,
    scopeName: m.scope_name,
    name: m.name,
    kind: Number(m.kind),
    startMs: Number(m.start_ns / NS_PER_MS),
    durationMs: Number(m.end_ns - m.start_ns) / 1_000_000,
    statusCode: Number(m.status_code),
    statusMessage: m.status_message,
    attributes,
  };
}

function generateToken(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return INGEST_TOKEN_PREFIX + Buffer.from(bytes).toString("hex");
}

export function tokenPreview(token: string): string {
  return `${token.slice(0, INGEST_TOKEN_PREFIX.length + 6)}…`;
}
