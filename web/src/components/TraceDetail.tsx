import { useState } from "react";
import { getTrace } from "../api";
import { usePolling } from "../hooks";
import { overviewPath } from "../router";
import { formatDuration, formatTime, spanKindLabel } from "../utils";
import type { SpanSummary } from "../types";

interface Props {
  traceId: string;
}

interface Row {
  span: SpanSummary;
  depth: number;
}

/** Depth-first order with children under their parent; orphans become roots. */
function layout(spans: SpanSummary[]): Row[] {
  const byParent = new Map<string | null, SpanSummary[]>();
  const ids = new Set(spans.map((s) => s.spanId));
  for (const span of spans) {
    const parent = span.parentSpanId && ids.has(span.parentSpanId) ? span.parentSpanId : null;
    const list = byParent.get(parent) ?? [];
    list.push(span);
    byParent.set(parent, list);
  }
  const rows: Row[] = [];
  const visit = (parent: string | null, depth: number) => {
    const children = byParent.get(parent) ?? [];
    children.sort((a, b) => a.startMs - b.startMs);
    for (const child of children) {
      rows.push({ span: child, depth });
      visit(child.spanId, depth + 1);
    }
  };
  visit(null, 0);
  return rows;
}

export function TraceDetail({ traceId }: Props) {
  const trace = usePolling(() => getTrace(traceId));
  const [open, setOpen] = useState<string | null>(null);

  if (trace.error) {
    return (
      <main>
        <a href={overviewPath()}>← Overview</a>
        <div className="alert">{trace.error}</div>
      </main>
    );
  }
  if (!trace.data) return <main><p className="muted">Loading…</p></main>;

  const rows = layout(trace.data.spans);
  const start = Math.min(...trace.data.spans.map((s) => s.startMs));
  const end = Math.max(...trace.data.spans.map((s) => s.startMs + s.durationMs));
  const total = Math.max(end - start, 0.001);

  return (
    <main>
      <header className="header">
        <div>
          <a href={overviewPath()}>← Overview</a>
          <h1 className="mono">{traceId}</h1>
          <p className="muted">
            {trace.data.spans.length} spans · {formatDuration(total)} · started {formatTime(start)}
          </p>
        </div>
      </header>

      <section className="card waterfall">
        {rows.map(({ span, depth }) => {
          const left = ((span.startMs - start) / total) * 100;
          const width = Math.max((span.durationMs / total) * 100, 0.5);
          const key = `${span.traceId}:${span.spanId}`;
          const isOpen = open === key;
          return (
            <div key={key} className={`wf-row ${span.statusCode === 2 ? "row-error" : ""}`}>
              <button
                className="wf-label"
                style={{ paddingLeft: `${depth * 16 + 8}px` }}
                onClick={() => setOpen(isOpen ? null : key)}
                title={span.name}
              >
                <span className="muted">{span.serviceName}</span> {span.name}
              </button>
              <div className="wf-track">
                <div className="wf-bar" style={{ left: `${left}%`, width: `${width}%` }} />
              </div>
              <div className="wf-dur mono">{formatDuration(span.durationMs)}</div>
              {isOpen && (
                <div className="wf-detail">
                  <dl>
                    <dt>span id</dt><dd className="mono">{span.spanId}</dd>
                    <dt>parent</dt><dd className="mono">{span.parentSpanId ?? "—"}</dd>
                    <dt>kind</dt><dd>{spanKindLabel(span.kind)}</dd>
                    <dt>scope</dt><dd>{span.scopeName ?? "—"}</dd>
                    <dt>status</dt>
                    <dd>{["unset", "ok", "error"][span.statusCode] ?? span.statusCode}{span.statusMessage ? ` — ${span.statusMessage}` : ""}</dd>
                  </dl>
                  <pre>{JSON.stringify(span.attributes ?? {}, null, 2)}</pre>
                </div>
              )}
            </div>
          );
        })}
      </section>
    </main>
  );
}
