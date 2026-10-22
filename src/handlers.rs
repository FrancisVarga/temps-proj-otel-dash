//! HTTP handlers and router assembly.
//!
//! Two kinds of callers reach this plugin:
//!
//! * Temps console users, proxied by Temps with a verified identity. The SDK
//!   middleware turns that into [`AuthenticatedCaller`]; mutating routes
//!   additionally require an admin role.
//! * OTLP exporters hitting `/v1/*`. Those routes are declared `public_paths`
//!   in the manifest, so Temps does not gate them and the plugin checks its
//!   own ingest token instead. The token travels in [`INGEST_TOKEN_HEADER`]:
//!   the Temps proxy strips `Authorization` and `Cookie` before forwarding to
//!   any plugin, so a bearer token would never arrive.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use include_dir::Dir;
use subtle::ConstantTimeEq;
use temps_plugin_sdk::prelude::{AuthenticatedCaller, PluginRole};
use tokio::sync::RwLock;

use crate::db::{token_preview, SpanStore, StoreError};
use crate::ingest::{IngestQueue, QueueFull};
use crate::otlp::{self, ExportTraceServiceRequest};
use crate::types::*;

/// Largest OTLP/HTTP request body accepted. Exporters default to far smaller
/// batches; this is a ceiling against runaway clients, not a target.
pub const MAX_INGEST_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Header carrying the ingest token. Must not start with `x-temps-` (reserved
/// and stripped by the proxy) and must not be `Authorization` (also stripped).
pub const INGEST_TOKEN_HEADER: &str = "x-otel-dash-token";
const RETRY_AFTER_SECS: &str = "1";

// ============================================================================
// State
// ============================================================================

/// Settings and secrets the hot path reads on every request. Cached in memory
/// and refreshed on write so ingest never waits on SQLite.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub settings: PluginSettings,
    pub ingest_token: String,
}

#[derive(Clone)]
pub struct AppState {
    pub store: SpanStore,
    pub queue: Arc<IngestQueue>,
    pub config: Arc<RwLock<RuntimeConfig>>,
    /// Public base URL of this plugin, when Temps told us one.
    pub mount_url: Option<String>,
}

impl AppState {
    pub async fn load(
        store: SpanStore,
        queue: Arc<IngestQueue>,
        mount_url: Option<String>,
    ) -> Result<Self, StoreError> {
        let config = RuntimeConfig {
            settings: store.get_settings().await?,
            ingest_token: store.ingest_token().await?,
        };
        Ok(Self {
            store,
            queue,
            config: Arc::new(RwLock::new(config)),
            mount_url,
        })
    }

    fn ingest_url(&self) -> Option<String> {
        self.mount_url
            .as_ref()
            .map(|base| format!("{base}/v1/traces"))
    }
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    BadRequest(String),
    #[error("Missing or invalid ingest token")]
    Unauthorized,
    #[error("This action requires an administrator")]
    Forbidden,
    #[error("Not found")]
    NotFound,
    #[error("Unsupported content type: {0}. Send OTLP/JSON (application/json).")]
    UnsupportedMediaType(String),
    #[error("Ingest is disabled in the plugin settings")]
    IngestDisabled,
    #[error(transparent)]
    Overloaded(#[from] QueueFull),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            AppError::IngestDisabled | AppError::Overloaded(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        match &self {
            AppError::Store(_) => tracing::error!(error = %self, "request failed"),
            _ => tracing::debug!(error = %self, status = status.as_u16(), "request rejected"),
        }
        let mut response = (
            status,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response();
        if matches!(self, AppError::Overloaded(_) | AppError::IngestDisabled) {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from_static(RETRY_AFTER_SECS),
            );
        }
        response
    }
}

fn require_admin(caller: &AuthenticatedCaller) -> Result<(), AppError> {
    match caller.role {
        PluginRole::Admin | PluginRole::PlatformAdmin => Ok(()),
        _ => Err(AppError::Forbidden),
    }
}

// ============================================================================
// Router
// ============================================================================

pub fn build_router(state: AppState) -> Router {
    let ingest = Router::new()
        .route("/v1/traces", post(ingest_traces))
        .layer(DefaultBodyLimit::max(MAX_INGEST_BODY_BYTES));

    Router::new()
        .merge(ingest)
        .route("/stats", get(get_stats))
        .route("/spans", get(list_spans))
        .route("/traces/{trace_id}", get(get_trace))
        .route("/services", get(list_services))
        .route("/settings", get(get_settings).patch(update_settings))
        .route("/settings/token", get(reveal_token).post(rotate_token))
        .route("/ui", get(redirect_to_ui))
        .route("/ui/", get(serve_ui_index))
        .route("/ui/{*path}", get(serve_ui_asset))
        .with_state(state)
}

// ============================================================================
// Ingest
// ============================================================================

/// OTLP/HTTP trace export endpoint (JSON encoding).
#[utoipa::path(
    post,
    path = "/v1/traces",
    tag = "Ingest",
    request_body(content_type = "application/json", description = "OTLP ExportTraceServiceRequest"),
    responses(
        (status = 200, description = "Accepted", body = ExportTraceResponse),
        (status = 400, description = "Malformed OTLP/JSON"),
        (status = 401, description = "Missing or invalid ingest token"),
        (status = 415, description = "Only application/json is supported"),
        (status = 503, description = "Ingest disabled or queue full — retry later"),
    ),
    params(("X-Otel-Dash-Token" = String, Header, description = "Ingest token from Settings")),
    security(())
)]
pub async fn ingest_traces(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ExportTraceResponse>, AppError> {
    {
        let config = state.config.read().await;
        if !config.settings.ingest_enabled {
            return Err(AppError::IngestDisabled);
        }
        authorize_ingest(&headers, &config.ingest_token)?;
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/json") {
        return Err(AppError::UnsupportedMediaType(content_type.to_string()));
    }

    let request: ExportTraceServiceRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid OTLP/JSON: {e}")))?;
    let flat = otlp::flatten(request);
    let accepted = flat.rows.len();

    state.queue.enqueue(flat.rows)?;

    tracing::debug!(accepted, rejected = flat.rejected, "trace export received");
    Ok(Json(ExportTraceResponse {
        partial_success: (flat.rejected > 0).then(|| PartialSuccess {
            rejected_spans: flat.rejected,
            error_message: "spans with invalid ids or timestamps were dropped".into(),
        }),
    }))
}

fn authorize_ingest(headers: &HeaderMap, expected: &str) -> Result<(), AppError> {
    let provided = headers
        .get(INGEST_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(AppError::Unauthorized)?;
    if bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

// ============================================================================
// Dashboard queries
// ============================================================================

#[utoipa::path(get, path = "/stats", tag = "Dashboard",
    responses((status = 200, body = StatsResponse)), security(()))]
pub async fn get_stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, AppError> {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    let counts = state.store.counts(now_ns).await?;
    let queue = state.queue.stats();
    Ok(Json(StatsResponse {
        total_spans: counts.total_spans,
        total_traces: counts.total_traces,
        total_services: counts.total_services,
        spans_last_hour: counts.spans_last_hour,
        accepted_total: queue.accepted_total,
        dropped_total: queue.dropped_total,
        failed_total: queue.failed_total,
        queue_depth: queue.queue_depth,
        queue_capacity: queue.queue_capacity,
        last_ingest_ms: queue.last_ingest_ms,
    }))
}

#[utoipa::path(get, path = "/spans", tag = "Dashboard", params(SpanQuery),
    responses((status = 200, body = Vec<SpanSummary>)), security(()))]
pub async fn list_spans(
    State(state): State<AppState>,
    Query(query): Query<SpanQuery>,
) -> Result<Json<Vec<SpanSummary>>, AppError> {
    Ok(Json(state.store.recent_spans(&query).await?))
}

#[utoipa::path(get, path = "/traces/{trace_id}", tag = "Dashboard",
    params(("trace_id" = String, Path, description = "32 hex character trace id")),
    responses((status = 200, body = TraceDetail), (status = 404, description = "Unknown trace")),
    security(()))]
pub async fn get_trace(
    State(state): State<AppState>,
    Path(trace_id): Path<String>,
) -> Result<Json<TraceDetail>, AppError> {
    let trace_id = trace_id.to_ascii_lowercase();
    if trace_id.len() != 32 || !trace_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(
            "trace id must be 32 hex characters".into(),
        ));
    }
    state
        .store
        .trace(&trace_id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[utoipa::path(get, path = "/services", tag = "Dashboard",
    responses((status = 200, body = Vec<ServiceSummary>)), security(()))]
pub async fn list_services(
    State(state): State<AppState>,
) -> Result<Json<Vec<ServiceSummary>>, AppError> {
    Ok(Json(state.store.services().await?))
}

// ============================================================================
// Settings
// ============================================================================

#[utoipa::path(get, path = "/settings", tag = "Settings",
    responses((status = 200, body = SettingsView)), security(()))]
pub async fn get_settings(State(state): State<AppState>) -> Result<Json<SettingsView>, AppError> {
    let config = state.config.read().await;
    Ok(Json(SettingsView {
        settings: config.settings.clone(),
        ingest_url: state.ingest_url(),
        token_preview: token_preview(&config.ingest_token),
    }))
}

#[utoipa::path(patch, path = "/settings", tag = "Settings", request_body = UpdateSettings,
    responses((status = 200, body = SettingsView), (status = 401), (status = 403)), security(()))]
pub async fn update_settings(
    State(state): State<AppState>,
    caller: AuthenticatedCaller,
    Json(update): Json<UpdateSettings>,
) -> Result<Json<SettingsView>, AppError> {
    require_admin(&caller)?;
    let settings = state.store.update_settings(&update).await?;
    let mut config = state.config.write().await;
    config.settings = settings.clone();
    tracing::info!(user = caller.user_id, ?settings, "settings updated");
    Ok(Json(SettingsView {
        settings,
        ingest_url: state.ingest_url(),
        token_preview: token_preview(&config.ingest_token),
    }))
}

#[utoipa::path(get, path = "/settings/token", tag = "Settings",
    responses((status = 200, body = IngestTokenReveal), (status = 401), (status = 403)), security(()))]
pub async fn reveal_token(
    State(state): State<AppState>,
    caller: AuthenticatedCaller,
) -> Result<Json<IngestTokenReveal>, AppError> {
    require_admin(&caller)?;
    let token = state.config.read().await.ingest_token.clone();
    Ok(Json(IngestTokenReveal { token }))
}

#[utoipa::path(post, path = "/settings/token", tag = "Settings",
    responses((status = 200, body = IngestTokenReveal), (status = 401), (status = 403)), security(()))]
pub async fn rotate_token(
    State(state): State<AppState>,
    caller: AuthenticatedCaller,
) -> Result<Json<IngestTokenReveal>, AppError> {
    require_admin(&caller)?;
    let token = state.store.rotate_ingest_token().await?;
    state.config.write().await.ingest_token = token.clone();
    tracing::warn!(user = caller.user_id, "ingest token rotated");
    Ok(Json(IngestTokenReveal { token }))
}

// ============================================================================
// Embedded UI
// ============================================================================

pub async fn redirect_to_ui() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, "ui/")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn serve_ui_index() -> Response {
    serve_embedded_file(crate::ui_dist(), "index.html")
}

pub async fn serve_ui_asset(Path(path): Path<String>) -> Response {
    let dist = crate::ui_dist();
    if dist.get_file(&path).is_some() {
        return serve_embedded_file(dist, &path);
    }
    // SPA fallback: unknown paths render the app shell.
    serve_embedded_file(dist, "index.html")
}

fn serve_embedded_file(dist: &Dir<'static>, path: &str) -> Response {
    match dist.get_file(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            let cache = if path == "index.html" {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, cache)
                .body(Body::from(file.contents()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const TRACES: &str = r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"web"}}]},"scopeSpans":[{"spans":[
        {"traceId":"5b8efff798038103d269b633813fc60c","spanId":"eee19b7ec3c1b174","name":"GET /","kind":2,"startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000100000000"},
        {"traceId":"bad","spanId":"eee19b7ec3c1b174","name":"x","startTimeUnixNano":"1","endTimeUnixNano":"2"}
    ]}]}]}"#;

    async fn app() -> (Router, AppState, TempDir) {
        let dir = TempDir::new().expect("temp dir");
        let store = SpanStore::open(dir.path()).await.expect("open");
        let queue = IngestQueue::start_with(store.clone(), 8, Duration::from_millis(10));
        let state = AppState::load(
            store,
            queue,
            Some("http://temps.local/api/x/otel-dash".into()),
        )
        .await
        .expect("state");
        (build_router(state.clone()), state, dir)
    }

    async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn export(token: Option<&str>, content_type: &str, body: &str) -> Request<Body> {
        let mut builder = Request::post("/v1/traces").header(header::CONTENT_TYPE, content_type);
        if let Some(token) = token {
            builder = builder.header(INGEST_TOKEN_HEADER, token);
        }
        builder.body(Body::from(body.to_string())).expect("request")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_requires_token_and_persists_spans() {
        let (app, state, _dir) = app().await;
        let token = state.config.read().await.ingest_token.clone();

        let (status, _) = send(&app, export(None, "application/json", TRACES)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = send(&app, export(Some("otd_wrong"), "application/json", TRACES)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = send(&app, export(Some(&token), "application/json", TRACES)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["partialSuccess"]["rejectedSpans"], 1);

        for _ in 0..100 {
            if state.store.counts(0).await.expect("counts").total_spans == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (status, body) = send(
            &app,
            Request::get("/stats").body(Body::empty()).expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["totalSpans"], 1);
        assert_eq!(body["acceptedTotal"], 1);
        assert_eq!(body["queueCapacity"], 8);

        let (status, body) = send(
            &app,
            Request::get("/spans?service=web")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["name"], "GET /");
        assert_eq!(body[0]["durationMs"], 100.0);

        let (status, body) = send(
            &app,
            Request::get("/traces/5B8EFFF798038103D269B633813FC60C")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["spans"].as_array().map(Vec::len), Some(1));

        let (status, _) = send(
            &app,
            Request::get("/traces/nope")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            &app,
            Request::get("/traces/00000000000000000000000000000001")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = send(
            &app,
            Request::get("/services").body(Body::empty()).expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["serviceName"], "web");
    }

    #[tokio::test]
    async fn ingest_rejects_bad_payloads_and_disabled_state() {
        let (app, state, _dir) = app().await;
        let token = state.config.read().await.ingest_token.clone();

        let (status, body) = send(&app, export(Some(&token), "application/x-protobuf", "")).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(body["error"]
            .as_str()
            .unwrap_or("")
            .contains("application/json"));

        let (status, _) = send(&app, export(Some(&token), "application/json", "{not json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        state.config.write().await.settings.ingest_enabled = false;
        let response = app
            .clone()
            .oneshot(export(Some(&token), "application/json", TRACES))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
    }

    #[tokio::test]
    async fn settings_are_readable_but_mutations_need_a_verified_caller() {
        let (app, _state, _dir) = app().await;

        let (status, body) = send(
            &app,
            Request::get("/settings").body(Body::empty()).expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ingestEnabled"], true);
        assert_eq!(body["retentionDays"], 7);
        assert_eq!(
            body["ingestUrl"],
            "http://temps.local/api/x/otel-dash/v1/traces"
        );
        assert!(body["tokenPreview"]
            .as_str()
            .unwrap_or("")
            .starts_with("otd_"));
        assert!(
            body.get("token").is_none(),
            "full token never leaks from GET /settings"
        );

        // No SDK-verified caller extension on these test requests → 401.
        let (status, _) = send(
            &app,
            Request::patch("/settings")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"retentionDays":3}"#))
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &app,
            Request::get("/settings/token")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &app,
            Request::post("/settings/token")
                .body(Body::empty())
                .expect("req"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ui_routes_serve_the_embedded_shell() {
        let (app, _state, _dir) = app().await;
        let response = app
            .clone()
            .oneshot(Request::get("/ui").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("ui/")
        );

        for path in ["/ui/", "/ui/some/deep/route"] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).expect("req"))
                .await
                .expect("resp");
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .starts_with("text/html"));
        }
    }

    #[test]
    fn token_comparison_is_exact() {
        let mut headers = HeaderMap::new();
        assert!(authorize_ingest(&headers, "otd_abc").is_err());
        headers.insert(INGEST_TOKEN_HEADER, "otd_abc".parse().expect("header"));
        assert!(authorize_ingest(&headers, "otd_abc").is_ok());
        assert!(authorize_ingest(&headers, "otd_abd").is_err());
        assert!(authorize_ingest(&headers, "otd_ab").is_err());
        headers.insert(INGEST_TOKEN_HEADER, "  ".parse().expect("header"));
        assert!(authorize_ingest(&headers, "otd_abc").is_err());
        // The proxy strips Authorization, so it must never be honoured either.
        headers.remove(INGEST_TOKEN_HEADER);
        headers.insert(
            header::AUTHORIZATION,
            "Bearer otd_abc".parse().expect("header"),
        );
        assert!(authorize_ingest(&headers, "otd_abc").is_err());
    }
}
