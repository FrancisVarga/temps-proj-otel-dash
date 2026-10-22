//! OpenTelemetry Dashboard — an external plugin for Temps.
//!
//! Receives OTLP/HTTP (JSON) trace exports from your services, stores spans in
//! a plugin-owned SQLite database, and renders services, recent spans and
//! trace detail inside the Temps console.
//!
//! ## Hot path
//!
//! ```text
//! exporter ─POST /v1/traces─▶ Temps proxy ─▶ unix socket ─▶ ingest_traces
//!                                                             │ parse + flatten (no DB)
//!                                                             ▼
//!                                                   bounded mpsc (back-pressure → 503)
//!                                                             │
//!                                                             ▼
//!                                              single writer task, batched transactions
//!                                                             │
//!                                                             ▼
//!                                                  SQLite (WAL, NORMAL sync, WITHOUT ROWID)
//! ```
//!
//! ## API (mounted at `/api/x/otel-dash`)
//!
//! ```text
//! POST  /v1/traces              OTLP/JSON trace export (X-Otel-Dash-Token header; public path)
//! GET   /stats                  Totals, queue depth, ingest counters
//! GET   /spans?limit&service&errorsOnly
//! GET   /traces/{trace_id}      All spans of one trace
//! GET   /services               Per-service counts and latency
//! GET   /settings               Settings + ingest URL + token preview
//! PATCH /settings               Update settings (admin)
//! GET   /settings/token         Reveal ingest token (admin)
//! POST  /settings/token         Rotate ingest token (admin)
//! GET   /ui/                    Embedded React UI
//! ```

mod db;
mod handlers;
mod ingest;
mod otlp;
mod types;

use std::time::Duration;

use include_dir::{include_dir, Dir};
use temps_plugin_sdk::prelude::*;

use crate::db::SpanStore;
use crate::handlers::AppState;
use crate::ingest::IngestQueue;

pub const PLUGIN_NAME: &str = "otel-dash";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const RETENTION_INTERVAL: Duration = Duration::from_secs(10 * 60);
const NS_PER_DAY: i64 = 86_400 * 1_000_000_000;

// ============================================================================
// OpenAPI
// ============================================================================

#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "OpenTelemetry Dashboard",
        version = "0.1.0",
        description = "OTLP trace ingest and a span/trace explorer for services deployed on Temps."
    ),
    paths(
        handlers::ingest_traces,
        handlers::get_stats,
        handlers::list_spans,
        handlers::get_trace,
        handlers::list_services,
        handlers::get_settings,
        handlers::update_settings,
        handlers::reveal_token,
        handlers::rotate_token,
    ),
    components(schemas(
        types::ExportTraceResponse,
        types::PartialSuccess,
        types::StatsResponse,
        types::SpanSummary,
        types::TraceDetail,
        types::ServiceSummary,
        types::PluginSettings,
        types::UpdateSettings,
        types::SettingsView,
        types::IngestTokenReveal,
    )),
    tags(
        (name = "Ingest",    description = "OTLP/HTTP endpoints for exporters"),
        (name = "Dashboard", description = "Read models for the UI"),
        (name = "Settings",  description = "Plugin configuration"),
    )
)]
struct OtelDashApiDoc;

// ============================================================================
// Embedded UI
// ============================================================================

static UI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

pub fn ui_dist() -> &'static Dir<'static> {
    &UI_DIST
}

// ============================================================================
// Plugin
// ============================================================================

#[derive(Default)]
struct OtelDashPlugin;

impl ExternalPlugin for OtelDashPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::builder(PLUGIN_NAME, PLUGIN_VERSION)
            .display_name("OpenTelemetry Dashboard")
            .description("Ingest OTLP traces from your services and explore spans, traces and service latency")
            .requires_db(false)
            .nav(NavEntry {
                label: "OpenTelemetry".into(),
                icon: "activity".into(),
                section: NavSection::Platform,
                path: format!("/{PLUGIN_NAME}"),
                order: 45,
            })
            // Exporters hold no Temps session; `/v1/*` authenticates itself
            // with the ingest token header (see `handlers::ingest_traces`).
            .public_path("/v1")
            .build()
    }

    fn router(&self, ctx: PluginContext) -> axum::Router {
        // `router()` runs inside the SDK's Tokio runtime: block_in_place avoids
        // deadlocking the executor while we do async setup.
        let state = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let store = SpanStore::open(ctx.data_dir()).await?;
                let queue = IngestQueue::start(store.clone());
                spawn_retention_task(store.clone());
                AppState::load(store, queue, ctx.mount_url()).await
            })
        })
        .expect("failed to initialise the OpenTelemetry Dashboard store");

        handlers::build_router(state)
    }

    fn openapi_schema(&self) -> Option<utoipa::openapi::OpenApi> {
        use utoipa::OpenApi as _;
        Some(OtelDashApiDoc::openapi())
    }

    fn on_start(&self, ctx: &PluginContext) -> Result<(), PluginSdkError> {
        tracing::info!(
            plugin = ctx.plugin_name(),
            version = PLUGIN_VERSION,
            data_dir = %ctx.data_dir().display(),
            ingest_url = ?ctx.mount_url().map(|base| format!("{base}/v1/traces")),
            "OpenTelemetry Dashboard started"
        );
        Ok(())
    }

    fn on_shutdown(&self) {
        tracing::info!("OpenTelemetry Dashboard shutting down");
    }
}

/// Periodically delete spans older than the configured retention window.
fn spawn_retention_task(store: SpanStore) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let retention_days = match store.get_settings().await {
                Ok(settings) => i64::from(settings.retention_days),
                Err(error) => {
                    tracing::warn!(%error, "retention: could not load settings, skipping run");
                    continue;
                }
            };
            let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let cutoff_ns = now_ns.saturating_sub(retention_days.saturating_mul(NS_PER_DAY));
            match store.prune_before(cutoff_ns).await {
                Ok(0) => {}
                Ok(removed) => {
                    tracing::info!(removed, retention_days, "retention: pruned old spans")
                }
                Err(error) => tracing::error!(%error, "retention: prune failed"),
            }
        }
    });
}

temps_plugin_sdk::main!(OtelDashPlugin);
