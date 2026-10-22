import { Overview } from "./components/Overview";
import { Settings } from "./components/Settings";
import { TraceDetail } from "./components/TraceDetail";
import { overviewPath, settingsPath, useRoute } from "./router";

export function App() {
  const route = useRoute();

  return (
    <div className="app">
      <nav className="topnav" aria-label="Plugin sections">
        <a href={overviewPath()} className={route.kind !== "settings" ? "active" : ""}>
          Overview
        </a>
        <a href={settingsPath()} className={route.kind === "settings" ? "active" : ""}>
          Settings
        </a>
      </nav>
      {route.kind === "overview" && <Overview />}
      {route.kind === "trace" && <TraceDetail traceId={route.traceId} />}
      {route.kind === "settings" && <Settings />}
    </div>
  );
}
