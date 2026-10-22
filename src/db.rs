//! SQLite persistence for spans and settings.
//!
//! Two connection pools share one WAL-mode database file in the plugin's
//! `data_dir`:
//!
//! * `writer` — exactly one connection. SQLite has a single writer anyway, so
//!   a pool of one removes lock contention and `SQLITE_BUSY` churn. All inserts
//!   and deletes run here in chunked transactions.
//! * `reader` — a small pool for dashboard queries. WAL lets readers proceed
//!   while the writer commits.
//!
//! Migrations are plain DDL executed at open; the schema is small enough that
//! a migration framework would be more code than the schema itself.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, SqlxSqliteConnector,
    Statement, TransactionTrait,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::otlp::SpanRow;
use crate::types::*;

// ============================================================================
// Entities
// ============================================================================

pub mod span {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "spans")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub trace_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub span_id: String,
        pub parent_span_id: Option<String>,
        pub service_name: String,
        pub scope_name: Option<String>,
        pub name: String,
        pub kind: i32,
        pub start_ns: i64,
        pub end_ns: i64,
        pub status_code: i32,
        pub status_message: Option<String>,
        pub attributes_json: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod setting {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "settings")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub key: String,
        pub value: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Failed to open SQLite database at {path}: {reason}")]
    Connect { path: String, reason: String },
    #[error("Migration failed: {0}")]
    Migration(String),
    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    #[error("Stored settings are corrupt: {0}")]
    CorruptSettings(String),
}

// ============================================================================
// Store
// ============================================================================

const DB_FILE: &str = "otel-dash.db";
const SETTINGS_KEY: &str = "plugin_settings";
const INGEST_TOKEN_KEY: &str = "ingest_token";
const INGEST_TOKEN_PREFIX: &str = "otd_";
/// Rows per `INSERT`. 12 columns × 400 rows stays well under SQLite's
/// 32 766 bound-parameter limit.
const INSERT_CHUNK: usize = 400;
const READ_POOL_SIZE: u32 = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const NS_PER_MS: i64 = 1_000_000;

#[derive(Clone)]
pub struct SpanStore {
    writer: Arc<DatabaseConnection>,
    reader: Arc<DatabaseConnection>,
}

impl SpanStore {
    /// Open (or create) the database in `data_dir`, apply pragmas and DDL,
    /// and make sure an ingest token exists.
    pub async fn open(data_dir: &Path) -> Result<Self, StoreError> {
        let db_path = data_dir.join(DB_FILE);
        let connect = |path: &Path| {
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .busy_timeout(BUSY_TIMEOUT)
                .foreign_keys(false)
                .pragma("temp_store", "memory")
                .pragma("cache_size", "-32768")
        };
        let map_err = |e: sqlx::Error| StoreError::Connect {
            path: db_path.display().to_string(),
            reason: e.to_string(),
        };

        let writer_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(connect(&db_path))
            .await
            .map_err(map_err)?;
        let reader_pool = SqlitePoolOptions::new()
            .max_connections(READ_POOL_SIZE)
            .connect_with(connect(&db_path))
            .await
            .map_err(map_err)?;

        let writer = SqlxSqliteConnector::from_sqlx_sqlite_pool(writer_pool);
        let reader = SqlxSqliteConnector::from_sqlx_sqlite_pool(reader_pool);

        Self::migrate(&writer).await?;

        let store = Self {
            writer: Arc::new(writer),
            reader: Arc::new(reader),
        };
        store.ensure_ingest_token().await?;

        tracing::info!(path = %db_path.display(), "span store opened");
        Ok(store)
    }

    async fn migrate(db: &DatabaseConnection) -> Result<(), StoreError> {
        const DDL: &[&str] = &[
            r#"CREATE TABLE IF NOT EXISTS spans (
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
            ) WITHOUT ROWID"#,
            "CREATE INDEX IF NOT EXISTS idx_spans_start ON spans (start_ns DESC)",
            "CREATE INDEX IF NOT EXISTS idx_spans_service_start ON spans (service_name, start_ns DESC)",
            "CREATE INDEX IF NOT EXISTS idx_spans_status_start ON spans (status_code, start_ns DESC)",
            r#"CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            )"#,
        ];
        for sql in DDL {
            db.execute(Statement::from_string(DatabaseBackend::Sqlite, *sql))
                .await
                .map_err(|e| StoreError::Migration(e.to_string()))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Spans — write path
    // ------------------------------------------------------------------------

    /// Insert rows in chunked transactions, ignoring duplicates (OTLP
    /// exporters retry, so re-delivery must be idempotent). Returns the number
    /// of rows actually written.
    pub async fn insert_spans(&self, rows: &[SpanRow]) -> Result<u64, StoreError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut written = 0u64;
        let txn = self.writer.begin().await?;
        for chunk in rows.chunks(INSERT_CHUNK) {
            let models = chunk.iter().map(|row| span::ActiveModel {
                trace_id: Set(row.trace_id.clone()),
                span_id: Set(row.span_id.clone()),
                parent_span_id: Set(row.parent_span_id.clone()),
                service_name: Set(row.service_name.clone()),
                scope_name: Set(row.scope_name.clone()),
                name: Set(row.name.clone()),
                kind: Set(row.kind),
                start_ns: Set(row.start_ns),
                end_ns: Set(row.end_ns),
                status_code: Set(row.status_code),
                status_message: Set(row.status_message.clone()),
                attributes_json: Set(row.attributes_json.clone()),
            });
            written += span::Entity::insert_many(models)
                .on_conflict(
                    OnConflict::columns([span::Column::TraceId, span::Column::SpanId])
                        .do_nothing()
                        .to_owned(),
                )
                .exec_without_returning(&txn)
                .await?;
        }
        txn.commit().await?;
        Ok(written)
    }

    /// Delete spans that started before `cutoff_ns`. Returns rows removed.
    pub async fn prune_before(&self, cutoff_ns: i64) -> Result<u64, StoreError> {
        let result = span::Entity::delete_many()
            .filter(span::Column::StartNs.lt(cutoff_ns))
            .exec(self.writer.as_ref())
            .await?;
        Ok(result.rows_affected)
    }

    // ------------------------------------------------------------------------
    // Spans — read path
    // ------------------------------------------------------------------------

    pub async fn recent_spans(&self, query: &SpanQuery) -> Result<Vec<SpanSummary>, StoreError> {
        let mut select = span::Entity::find()
            .order_by_desc(span::Column::StartNs)
            .limit(query.effective_limit());
        if let Some(service) = query.service.as_deref().filter(|s| !s.is_empty()) {
            select = select.filter(span::Column::ServiceName.eq(service));
        }
        if query.errors_only.unwrap_or(false) {
            select = select.filter(span::Column::StatusCode.eq(2));
        }
        let models = select.all(self.reader.as_ref()).await?;
        Ok(models.into_iter().map(to_summary).collect())
    }

    pub async fn trace(&self, trace_id: &str) -> Result<Option<TraceDetail>, StoreError> {
        let models = span::Entity::find()
            .filter(span::Column::TraceId.eq(trace_id))
            .order_by_asc(span::Column::StartNs)
            .all(self.reader.as_ref())
            .await?;
        if models.is_empty() {
            return Ok(None);
        }
        Ok(Some(TraceDetail {
            trace_id: trace_id.to_string(),
            spans: models.into_iter().map(to_summary).collect(),
        }))
    }

    pub async fn services(&self) -> Result<Vec<ServiceSummary>, StoreError> {
        let rows = self
            .reader
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                r#"SELECT service_name,
                          COUNT(*)                                  AS span_count,
                          COALESCE(SUM(status_code = 2), 0)         AS error_count,
                          COALESCE(AVG(end_ns - start_ns), 0.0)     AS avg_duration_ns
                   FROM spans
                   GROUP BY service_name
                   ORDER BY span_count DESC"#,
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ServiceSummary {
                    service_name: row.try_get("", "service_name")?,
                    span_count: row.try_get("", "span_count")?,
                    error_count: row.try_get("", "error_count")?,
                    avg_duration_ms: row.try_get::<f64>("", "avg_duration_ns")? / NS_PER_MS as f64,
                })
            })
            .collect()
    }

    /// Aggregate counters. `now_ns` is injected so tests are deterministic.
    pub async fn counts(&self, now_ns: i64) -> Result<SpanCounts, StoreError> {
        let totals = self
            .reader
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                r#"SELECT COUNT(*)                     AS total_spans,
                          COUNT(DISTINCT trace_id)     AS total_traces,
                          COUNT(DISTINCT service_name) AS total_services
                   FROM spans"#,
            ))
            .await?
            .ok_or_else(|| sea_orm::DbErr::RecordNotFound("aggregate row".into()))?;

        let hour_ago = now_ns - 3_600 * 1_000_000_000;
        let last_hour = span::Entity::find()
            .filter(span::Column::StartNs.gte(hour_ago))
            .count(self.reader.as_ref())
            .await?;

        Ok(SpanCounts {
            total_spans: totals.try_get("", "total_spans")?,
            total_traces: totals.try_get("", "total_traces")?,
            total_services: totals.try_get("", "total_services")?,
            spans_last_hour: i64::try_from(last_hour).unwrap_or(i64::MAX),
        })
    }

    // ------------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------------

    pub async fn get_settings(&self) -> Result<PluginSettings, StoreError> {
        match self.get_setting(SETTINGS_KEY).await? {
            Some(raw) => {
                serde_json::from_str(&raw).map_err(|e| StoreError::CorruptSettings(e.to_string()))
            }
            None => Ok(PluginSettings::default()),
        }
    }

    pub async fn update_settings(
        &self,
        update: &UpdateSettings,
    ) -> Result<PluginSettings, StoreError> {
        let current = self.get_settings().await?;
        let next = PluginSettings {
            ingest_enabled: update.ingest_enabled.unwrap_or(current.ingest_enabled),
            retention_days: update
                .retention_days
                .unwrap_or(current.retention_days)
                .clamp(1, PluginSettings::MAX_RETENTION_DAYS),
        };
        let raw =
            serde_json::to_string(&next).map_err(|e| StoreError::CorruptSettings(e.to_string()))?;
        self.put_setting(SETTINGS_KEY, &raw).await?;
        Ok(next)
    }

    pub async fn ingest_token(&self) -> Result<String, StoreError> {
        match self.get_setting(INGEST_TOKEN_KEY).await? {
            Some(token) => Ok(token),
            None => self.rotate_ingest_token().await,
        }
    }

    pub async fn rotate_ingest_token(&self) -> Result<String, StoreError> {
        let token = generate_token();
        self.put_setting(INGEST_TOKEN_KEY, &token).await?;
        Ok(token)
    }

    async fn ensure_ingest_token(&self) -> Result<(), StoreError> {
        self.ingest_token().await.map(|_| ())
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(setting::Entity::find_by_id(key)
            .one(self.reader.as_ref())
            .await?
            .map(|model| model.value))
    }

    async fn put_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        setting::Entity::insert(setting::ActiveModel {
            key: Set(key.to_string()),
            value: Set(value.to_string()),
        })
        .on_conflict(
            OnConflict::column(setting::Column::Key)
                .update_column(setting::Column::Value)
                .to_owned(),
        )
        .exec_without_returning(self.writer.as_ref())
        .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanCounts {
    pub total_spans: i64,
    pub total_traces: i64,
    pub total_services: i64,
    pub spans_last_hour: i64,
}

fn to_summary(model: span::Model) -> SpanSummary {
    SpanSummary {
        trace_id: model.trace_id,
        span_id: model.span_id,
        parent_span_id: model.parent_span_id,
        service_name: model.service_name,
        scope_name: model.scope_name,
        name: model.name,
        kind: model.kind,
        start_ms: model.start_ns / NS_PER_MS,
        duration_ms: (model.end_ns - model.start_ns) as f64 / NS_PER_MS as f64,
        status_code: model.status_code,
        status_message: model.status_message,
        attributes: serde_json::from_str(&model.attributes_json).unwrap_or(serde_json::Value::Null),
    }
}

/// 256 bits of randomness, hex encoded, with a recognisable prefix.
fn generate_token() -> String {
    format!(
        "{INGEST_TOKEN_PREFIX}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Short, non-secret preview of a token for display.
pub fn token_preview(token: &str) -> String {
    let visible = token
        .chars()
        .take(INGEST_TOKEN_PREFIX.len() + 6)
        .collect::<String>();
    format!("{visible}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    pub async fn test_store() -> (SpanStore, TempDir) {
        let dir = TempDir::new().expect("temp dir");
        let store = SpanStore::open(dir.path()).await.expect("open store");
        (store, dir)
    }

    pub fn row(
        trace: &str,
        span: &str,
        service: &str,
        start_ns: i64,
        duration_ns: i64,
        status: i32,
    ) -> SpanRow {
        SpanRow {
            trace_id: trace.into(),
            span_id: span.into(),
            parent_span_id: None,
            service_name: service.into(),
            scope_name: None,
            name: format!("op-{span}"),
            kind: 2,
            start_ns,
            end_ns: start_ns + duration_ns,
            status_code: status,
            status_message: None,
            attributes_json: r#"{"k":"v"}"#.into(),
        }
    }

    #[tokio::test]
    async fn insert_is_idempotent_and_queries_work() {
        let (store, _dir) = test_store().await;
        let rows = vec![
            row("t1", "s1", "api", 1_000 * NS_PER_MS, 50 * NS_PER_MS, 1),
            row("t1", "s2", "db", 1_010 * NS_PER_MS, 10 * NS_PER_MS, 2),
            row("t2", "s3", "api", 2_000 * NS_PER_MS, 100 * NS_PER_MS, 0),
        ];
        assert_eq!(store.insert_spans(&rows).await.expect("insert"), 3);
        assert_eq!(
            store.insert_spans(&rows).await.expect("re-insert"),
            0,
            "duplicates are ignored"
        );

        let recent = store
            .recent_spans(&SpanQuery::default())
            .await
            .expect("recent");
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].span_id, "s3", "newest first");
        assert_eq!(recent[0].start_ms, 2_000);
        assert_eq!(recent[0].duration_ms, 100.0);
        assert_eq!(recent[0].attributes["k"], "v");

        let api_only = store
            .recent_spans(&SpanQuery {
                service: Some("api".into()),
                ..Default::default()
            })
            .await
            .expect("filtered");
        assert_eq!(api_only.len(), 2);

        let errors = store
            .recent_spans(&SpanQuery {
                errors_only: Some(true),
                ..Default::default()
            })
            .await
            .expect("errors");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].span_id, "s2");

        let trace = store.trace("t1").await.expect("trace").expect("exists");
        assert_eq!(
            trace
                .spans
                .iter()
                .map(|s| s.span_id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2"]
        );
        assert!(store.trace("missing").await.expect("trace").is_none());

        let services = store.services().await.expect("services");
        assert_eq!(services[0].service_name, "api");
        assert_eq!(services[0].span_count, 2);
        assert_eq!(services[0].error_count, 0);
        assert_eq!(services[0].avg_duration_ms, 75.0);
        assert_eq!(services[1].error_count, 1);

        let counts = store.counts(2_500 * NS_PER_MS).await.expect("counts");
        assert_eq!(
            counts,
            SpanCounts {
                total_spans: 3,
                total_traces: 2,
                total_services: 2,
                spans_last_hour: 3
            }
        );
    }

    #[tokio::test]
    async fn large_batches_are_chunked() {
        let (store, _dir) = test_store().await;
        let rows = (0..(INSERT_CHUNK * 2 + 17))
            .map(|i| row(&format!("t{i}"), "s", "svc", i as i64, 1, 0))
            .collect::<Vec<_>>();
        assert_eq!(
            store.insert_spans(&rows).await.expect("insert"),
            rows.len() as u64
        );
        assert_eq!(
            store.counts(0).await.expect("counts").total_spans,
            rows.len() as i64
        );
    }

    #[tokio::test]
    async fn prune_removes_old_spans() {
        let (store, _dir) = test_store().await;
        store
            .insert_spans(&[
                row("t1", "old", "svc", 10, 1, 0),
                row("t2", "new", "svc", 100, 1, 0),
            ])
            .await
            .expect("insert");
        assert_eq!(store.prune_before(50).await.expect("prune"), 1);
        let left = store
            .recent_spans(&SpanQuery::default())
            .await
            .expect("recent");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].span_id, "new");
    }

    #[tokio::test]
    async fn settings_default_update_and_clamp() {
        let (store, _dir) = test_store().await;
        assert_eq!(
            store.get_settings().await.expect("defaults"),
            PluginSettings::default()
        );

        let updated = store
            .update_settings(&UpdateSettings {
                ingest_enabled: Some(false),
                retention_days: Some(9_999),
            })
            .await
            .expect("update");
        assert!(!updated.ingest_enabled);
        assert_eq!(updated.retention_days, PluginSettings::MAX_RETENTION_DAYS);
        assert_eq!(store.get_settings().await.expect("reload"), updated);

        let partial = store
            .update_settings(&UpdateSettings {
                retention_days: Some(0),
                ..Default::default()
            })
            .await
            .expect("partial");
        assert!(!partial.ingest_enabled, "untouched field is preserved");
        assert_eq!(partial.retention_days, 1);
    }

    #[tokio::test]
    async fn ingest_token_is_created_once_and_rotates() {
        let (store, dir) = test_store().await;
        let first = store.ingest_token().await.expect("token");
        assert!(first.starts_with(INGEST_TOKEN_PREFIX));
        assert_eq!(first.len(), INGEST_TOKEN_PREFIX.len() + 64);
        assert_eq!(
            store.ingest_token().await.expect("token"),
            first,
            "stable across calls"
        );

        let reopened = SpanStore::open(dir.path()).await.expect("reopen");
        assert_eq!(
            reopened.ingest_token().await.expect("token"),
            first,
            "survives restart"
        );

        let rotated = store.rotate_ingest_token().await.expect("rotate");
        assert_ne!(rotated, first);
        assert_eq!(store.ingest_token().await.expect("token"), rotated);
        assert_eq!(
            token_preview(&rotated).len(),
            INGEST_TOKEN_PREFIX.len() + 6 + '…'.len_utf8()
        );
    }
}
