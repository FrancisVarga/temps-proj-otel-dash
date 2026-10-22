// OpenTelemetry Dashboard — TypeScript build of the Temps plugin.
// See ../README.md for the architecture; the Rust build in the repository root
// shares the same HTTP API, storage schema and UI bundle.

import { createManifest, runPlugin, type PluginContext, type TempsPlugin } from "@temps-sdk/plugin";
import pkg from "../package.json";
import { embeddedAssets } from "./_embedded_ui";
import { SpanStore } from "./db";
import { createAppState, createHandler } from "./handlers";
import { IngestQueue } from "./ingest";

const RETENTION_INTERVAL_MS = 10 * 60 * 1000;
const NS_PER_DAY = 86_400n * 1_000_000_000n;

export function manifest() {
  return createManifest(pkg.temps.name, pkg.version)
    .displayName(pkg.temps.title)
    .description(pkg.description)
    .requiresDb(false)
    .nav({ label: "OpenTelemetry", icon: "activity", section: "platform", path: `/${pkg.temps.name}`, order: 45 })
    // Exporters hold no Temps session; the handler checks X-Otel-Dash-Token itself.
    .publicPath("/v1")
    .build();
}

let store: SpanStore | undefined;
let queue: IngestQueue | undefined;
let retention: ReturnType<typeof setInterval> | undefined;

function log(level: string, message: string, extra: Record<string, unknown> = {}) {
  console.error(JSON.stringify({ level, message, ...extra }));
}

export const plugin: TempsPlugin = {
  manifest,

  embeddedUiAssets: () => embeddedAssets,

  handler(ctx: PluginContext) {
    store = SpanStore.open(ctx.dataDir);
    queue = new IngestQueue(store);
    const state = createAppState(store, queue, ctx);
    const s = store;
    retention = setInterval(() => {
      try {
        const days = BigInt(s.getSettings().retentionDays);
        const removed = s.pruneBefore(BigInt(Date.now()) * 1_000_000n - days * NS_PER_DAY);
        if (removed > 0) log("info", "retention: pruned old spans", { removed, retention_days: Number(days) });
      } catch (error) {
        log("error", "retention: prune failed", { reason: error instanceof Error ? error.message : String(error) });
      }
    }, RETENTION_INTERVAL_MS);
    retention.unref?.();
    return createHandler(state);
  },

  onStart(ctx) {
    log("info", "OpenTelemetry Dashboard started", {
      version: pkg.version,
      data_dir: ctx.dataDir,
      ingest_url: ctx.mountUrl ? `${ctx.mountUrl}/v1/traces` : null,
    });
  },

  onShutdown() {
    if (retention) clearInterval(retention);
    queue?.close();
    store?.close();
    log("info", "OpenTelemetry Dashboard stopped");
  },
};

if (import.meta.main) await runPlugin(plugin);
