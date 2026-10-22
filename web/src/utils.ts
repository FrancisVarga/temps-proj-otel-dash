export const SPAN_KIND_LABELS = ["unspecified", "internal", "server", "client", "producer", "consumer"] as const;

export function spanKindLabel(kind: number): string {
  return SPAN_KIND_LABELS[kind] ?? "unspecified";
}

export function formatDuration(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
  if (ms < 1000) return `${ms.toFixed(ms < 10 ? 2 : 0)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

export function formatCount(n: number): string {
  return new Intl.NumberFormat().format(n);
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, { hour12: false });
}

export function formatRelative(ms: number | null, now = Date.now()): string {
  if (ms === null) return "never";
  const delta = Math.max(0, now - ms);
  if (delta < 5_000) return "just now";
  if (delta < 60_000) return `${Math.floor(delta / 1000)} s ago`;
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} min ago`;
  return `${Math.floor(delta / 3_600_000)} h ago`;
}

export function shortId(id: string, chars = 8): string {
  return id.length > chars ? id.slice(0, chars) : id;
}
