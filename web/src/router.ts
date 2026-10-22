import { useCallback, useSyncExternalStore } from "react";

// Hash routing: the plugin runs inside an iframe and must not touch the
// console's own URL.

export type Route =
  | { kind: "overview" }
  | { kind: "trace"; traceId: string }
  | { kind: "settings" };

function parseHash(hash: string): Route {
  const path = hash.replace(/^#\/?/, "");
  const trace = path.match(/^traces\/([0-9a-fA-F]{32})$/);
  if (trace) return { kind: "trace", traceId: trace[1].toLowerCase() };
  if (path === "settings") return { kind: "settings" };
  return { kind: "overview" };
}

export const overviewPath = () => "#/";
export const tracePath = (traceId: string) => `#/traces/${traceId}`;
export const settingsPath = () => "#/settings";

export function navigate(hash: string): void {
  window.location.hash = hash;
}

// useSyncExternalStore compares snapshots with Object.is, so the parsed route
// is cached per hash string to avoid an infinite render loop.
let cachedHash = "";
let cachedRoute: Route = { kind: "overview" };

function getSnapshot(): Route {
  const hash = window.location.hash;
  if (hash !== cachedHash) {
    cachedHash = hash;
    cachedRoute = parseHash(hash);
  }
  return cachedRoute;
}

function subscribe(callback: () => void): () => void {
  window.addEventListener("hashchange", callback);
  return () => window.removeEventListener("hashchange", callback);
}

export function useRoute(): Route {
  return useSyncExternalStore(subscribe, getSnapshot);
}

export function useNavigate() {
  const goToOverview = useCallback(() => navigate(overviewPath()), []);
  const goToTrace = useCallback((traceId: string) => navigate(tracePath(traceId)), []);
  const goToSettings = useCallback(() => navigate(settingsPath()), []);
  return { goToOverview, goToTrace, goToSettings };
}
