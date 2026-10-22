import { useState } from "react";
import { getServices, getSpans, getStats } from "../api";
import { usePolling } from "../hooks";
import { tracePath } from "../router";
import { formatCount, formatDuration, formatRelative, formatTime, shortId, spanKindLabel } from "../utils";
import { StatCard } from "./StatCard";

const REFRESH_MS = 5_000;

export function Overview() {
  const [service, setService] = useState<string>("");
  const [errorsOnly, setErrorsOnly] = useState(false);

  const stats = usePolling(getStats, REFRESH_MS);
  const services = usePolling(getServices, REFRESH_MS);
  const spans = usePolling(() => getSpans({ limit: 100, service: service || undefined, errorsOnly }), REFRESH_MS);

  const error = stats.error ?? services.error ?? spans.error;
  const s = stats.data;
  const dropped = s ? s.droppedTotal + s.failedTotal : 0;

  return (
    <main>
      <header className="header">
        <div>
          <h1>OpenTelemetry</h1>
          <p className="muted">
            Last ingest {formatRelative(s?.lastIngestMs ?? null)} · refreshes every {REFRESH_MS / 1000}s
          </p>
        </div>
        <button className="btn" onClick={() => { stats.reload(); services.reload(); spans.reload(); }}>
          Refresh
        </button>
      </header>

      {error && <div className="alert">{error}</div>}

      <section className="stats">
        <StatCard label="Spans" value={s ? formatCount(s.totalSpans) : "—"} hint={s ? `${formatCount(s.spansLastHour)} in the last hour` : undefined} />
        <StatCard label="Traces" value={s ? formatCount(s.totalTraces) : "—"} />
        <StatCard label="Services" value={s ? formatCount(s.totalServices) : "—"} />
        <StatCard
          label="Write queue"
          value={s ? `${s.queueDepth} / ${s.queueCapacity}` : "—"}
          hint={s ? `${formatCount(s.acceptedTotal)} accepted since start` : undefined}
          tone={s && s.queueDepth > s.queueCapacity * 0.8 ? "warn" : "default"}
        />
        <StatCard
          label="Dropped"
          value={s ? formatCount(dropped) : "—"}
          hint="queue full or write failed"
          tone={dropped > 0 ? "danger" : "default"}
        />
      </section>

      <section className="card">
        <div className="card-head">
          <h2>Services</h2>
        </div>
        {services.data && services.data.length === 0 && (
          <p className="empty">
            No spans yet. Point an OTLP/HTTP exporter at the ingest URL shown under <a href="#/settings">Settings</a>.
          </p>
        )}
        {services.data && services.data.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Service</th>
                <th className="num">Spans</th>
                <th className="num">Errors</th>
                <th className="num">Avg duration</th>
              </tr>
            </thead>
            <tbody>
              {services.data.map((svc) => (
                <tr
                  key={svc.serviceName}
                  className={`clickable ${service === svc.serviceName ? "selected" : ""}`}
                  onClick={() => setService(service === svc.serviceName ? "" : svc.serviceName)}
                >
                  <td>{svc.serviceName}</td>
                  <td className="num">{formatCount(svc.spanCount)}</td>
                  <td className={`num ${svc.errorCount > 0 ? "danger" : ""}`}>{formatCount(svc.errorCount)}</td>
                  <td className="num">{formatDuration(svc.avgDurationMs)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="card">
        <div className="card-head">
          <h2>Recent spans{service ? ` · ${service}` : ""}</h2>
          <label className="toggle">
            <input type="checkbox" checked={errorsOnly} onChange={(e) => setErrorsOnly(e.target.checked)} />
            errors only
          </label>
        </div>
        {spans.data && spans.data.length === 0 && <p className="empty">Nothing matches.</p>}
        {spans.data && spans.data.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Time</th>
                <th>Service</th>
                <th>Span</th>
                <th>Kind</th>
                <th className="num">Duration</th>
                <th>Trace</th>
              </tr>
            </thead>
            <tbody>
              {spans.data.map((span) => (
                <tr key={`${span.traceId}:${span.spanId}`} className={span.statusCode === 2 ? "row-error" : ""}>
                  <td className="mono">{formatTime(span.startMs)}</td>
                  <td>{span.serviceName}</td>
                  <td title={span.statusMessage ?? undefined}>{span.name}</td>
                  <td className="muted">{spanKindLabel(span.kind)}</td>
                  <td className="num mono">{formatDuration(span.durationMs)}</td>
                  <td>
                    <a className="mono" href={tracePath(span.traceId)}>{shortId(span.traceId)}</a>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </main>
  );
}
