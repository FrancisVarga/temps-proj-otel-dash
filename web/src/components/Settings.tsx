import { useState } from "react";
import { getSettings, revealToken, rotateToken, updateSettings } from "../api";
import { usePolling } from "../hooks";
import type { SettingsView } from "../types";

export function Settings() {
  const settings = usePolling(getSettings);
  const [token, setToken] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async (label: string, action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await action();
      setNotice(label);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const save = (update: Partial<SettingsView>) =>
    run("Settings saved", async () => {
      await updateSettings({ ingestEnabled: update.ingestEnabled, retentionDays: update.retentionDays });
      settings.reload();
    });

  const s = settings.data;
  // Temps passes the plugin its internal bind address (e.g. http://127.0.0.1:3000),
  // which exporters outside the host cannot reach. The console origin is the
  // public URL, so prefer it whenever the reported URL is loopback or missing.
  const ingestUrl =
    s?.ingestUrl && !/^https?:\/\/(127\.|localhost|0\.0\.0\.0|\[::1\])/i.test(s.ingestUrl)
      ? s.ingestUrl
      : `${window.location.origin}/api/x/otel-dash/v1/traces`;

  return (
    <main>
      <header className="header">
        <div>
          <h1>Settings</h1>
          <p className="muted">Changing settings or handling the token requires a Temps administrator.</p>
        </div>
      </header>

      {settings.error && <div className="alert">{settings.error}</div>}
      {error && <div className="alert">{error}</div>}
      {notice && <div className="notice">{notice}</div>}

      {s && (
        <>
          <section className="card">
            <div className="card-head"><h2>Ingest endpoint</h2></div>
            <div className="card-body">
              <p>Configure your OTLP/HTTP exporter with JSON encoding:</p>
              <pre>{`OTEL_EXPORTER_OTLP_PROTOCOL=http/json
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=${ingestUrl}
OTEL_EXPORTER_OTLP_HEADERS="X-Otel-Dash-Token=${token ?? s.tokenPreview}"`}</pre>
              <div className="actions">
                <button
                  className="btn"
                  disabled={busy || token !== null}
                  onClick={() => run("Token revealed", async () => setToken((await revealToken()).token))}
                >
                  Reveal token
                </button>
                <button
                  className="btn btn-danger"
                  disabled={busy}
                  onClick={() => {
                    if (!window.confirm("Rotate the ingest token? Every exporter must be updated.")) return;
                    void run("Token rotated — update your exporters", async () => setToken((await rotateToken()).token));
                  }}
                >
                  Rotate token
                </button>
              </div>
            </div>
          </section>

          <section className="card">
            <div className="card-head"><h2>Ingest</h2></div>
            <div className="card-body">
              <label className="toggle">
                <input
                  type="checkbox"
                  checked={s.ingestEnabled}
                  disabled={busy}
                  onChange={(e) => void save({ ingestEnabled: e.target.checked })}
                />
                Accept OTLP exports (exporters receive 503 and retry while off)
              </label>
            </div>
          </section>

          <section className="card">
            <div className="card-head"><h2>Retention</h2></div>
            <div className="card-body">
              <RetentionForm value={s.retentionDays} busy={busy} onSave={(days) => void save({ retentionDays: days })} />
            </div>
          </section>
        </>
      )}
    </main>
  );
}

function RetentionForm({ value, busy, onSave }: { value: number; busy: boolean; onSave: (days: number) => void }) {
  const [days, setDays] = useState(String(value));
  const parsed = Number(days);
  const valid = Number.isInteger(parsed) && parsed >= 1 && parsed <= 365;
  return (
    <form
      className="inline-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (valid) onSave(parsed);
      }}
    >
      <label>
        Keep spans for
        <input type="number" min={1} max={365} value={days} onChange={(e) => setDays(e.target.value)} />
        days
      </label>
      <button className="btn" type="submit" disabled={busy || !valid || parsed === value}>
        Save
      </button>
    </form>
  );
}
