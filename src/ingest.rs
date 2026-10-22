//! Batched, back-pressured write queue between the OTLP handlers and SQLite.
//!
//! Handlers never touch the database. They push already-flattened rows into a
//! bounded channel and answer immediately; one writer task drains the channel
//! and commits in large transactions. This turns N small inserts per request
//! into a handful of transactions per second, which is where SQLite is fast.
//!
//! When the channel is full the handler answers 503 and the exporter retries,
//! which is the OTLP contract for transient overload.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::db::SpanStore;
use crate::otlp::SpanRow;

/// Default number of pending batches (not spans) the queue holds.
pub const DEFAULT_QUEUE_CAPACITY: usize = 1_024;
/// Default time the writer waits to fill a batch before committing.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(200);
/// Spans per transaction before the writer commits regardless of time.
pub const MAX_BATCH_SPANS: usize = 4_000;

#[derive(Debug, thiserror::Error)]
#[error("ingest queue is full ({capacity} batches pending)")]
pub struct QueueFull {
    pub capacity: usize,
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    /// Unix milliseconds of the last accepted batch; 0 means never.
    last_ingest_ms: AtomicI64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    pub accepted_total: u64,
    pub dropped_total: u64,
    pub failed_total: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub last_ingest_ms: Option<i64>,
}

pub struct IngestQueue {
    tx: mpsc::Sender<Vec<SpanRow>>,
    capacity: usize,
    counters: Arc<Counters>,
}

impl IngestQueue {
    /// Start the writer task with default tuning.
    pub fn start(store: SpanStore) -> Arc<Self> {
        Self::start_with(store, DEFAULT_QUEUE_CAPACITY, DEFAULT_FLUSH_INTERVAL)
    }

    pub fn start_with(store: SpanStore, capacity: usize, flush_interval: Duration) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(capacity);
        let counters = Arc::new(Counters::default());
        tokio::spawn(writer_loop(
            store,
            rx,
            Arc::clone(&counters),
            flush_interval,
        ));
        Arc::new(Self {
            tx,
            capacity,
            counters,
        })
    }

    /// Hand a batch to the writer without waiting. Empty batches are a no-op.
    pub fn enqueue(&self, rows: Vec<SpanRow>) -> Result<(), QueueFull> {
        if rows.is_empty() {
            return Ok(());
        }
        let count = rows.len() as u64;
        match self.tx.try_send(rows) {
            Ok(()) => {
                self.counters.accepted.fetch_add(count, Ordering::Relaxed);
                self.counters
                    .last_ingest_ms
                    .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) | Err(mpsc::error::TrySendError::Closed(_)) => {
                self.counters.dropped.fetch_add(count, Ordering::Relaxed);
                Err(QueueFull {
                    capacity: self.capacity,
                })
            }
        }
    }

    pub fn stats(&self) -> QueueStats {
        let last = self.counters.last_ingest_ms.load(Ordering::Relaxed);
        QueueStats {
            accepted_total: self.counters.accepted.load(Ordering::Relaxed),
            dropped_total: self.counters.dropped.load(Ordering::Relaxed),
            failed_total: self.counters.failed.load(Ordering::Relaxed),
            queue_depth: self.capacity.saturating_sub(self.tx.capacity()),
            queue_capacity: self.capacity,
            last_ingest_ms: (last > 0).then_some(last),
        }
    }
}

async fn writer_loop(
    store: SpanStore,
    mut rx: mpsc::Receiver<Vec<SpanRow>>,
    counters: Arc<Counters>,
    flush_interval: Duration,
) {
    let mut batch: Vec<SpanRow> = Vec::with_capacity(MAX_BATCH_SPANS);

    while let Some(first) = rx.recv().await {
        batch.extend(first);
        let deadline = Instant::now() + flush_interval;

        // Keep pulling until the batch is big enough or the deadline passes.
        while batch.len() < MAX_BATCH_SPANS {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(more)) => batch.extend(more),
                Ok(None) => break,
                Err(_elapsed) => break,
            }
        }

        flush(&store, &mut batch, &counters).await;
    }

    // Channel closed: persist whatever is left.
    flush(&store, &mut batch, &counters).await;
    tracing::info!("ingest writer stopped");
}

async fn flush(store: &SpanStore, batch: &mut Vec<SpanRow>, counters: &Counters) {
    if batch.is_empty() {
        return;
    }
    let started = Instant::now();
    match store.insert_spans(batch).await {
        Ok(written) => tracing::debug!(
            spans = batch.len(),
            written,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "flushed span batch"
        ),
        Err(error) => {
            counters
                .failed
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            tracing::error!(spans = batch.len(), %error, "failed to persist span batch");
        }
    }
    batch.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SpanQuery;
    use tempfile::TempDir;

    fn row(i: i64) -> SpanRow {
        SpanRow {
            trace_id: format!("{i:032x}"),
            span_id: format!("{i:016x}"),
            parent_span_id: None,
            service_name: "svc".into(),
            scope_name: None,
            name: "op".into(),
            kind: 1,
            start_ns: i,
            end_ns: i + 1,
            status_code: 0,
            status_message: None,
            attributes_json: "{}".into(),
        }
    }

    async fn wait_for_spans(store: &SpanStore, expected: usize) {
        for _ in 0..100 {
            let n = store
                .recent_spans(&SpanQuery {
                    limit: Some(500),
                    ..Default::default()
                })
                .await
                .expect("query")
                .len();
            if n >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("writer did not persist {expected} spans in time");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batches_are_persisted_and_counted() {
        let dir = TempDir::new().expect("temp dir");
        let store = SpanStore::open(dir.path()).await.expect("open");
        let queue = IngestQueue::start_with(store.clone(), 16, Duration::from_millis(20));

        queue.enqueue(vec![row(1), row(2)]).expect("enqueue");
        queue.enqueue(vec![row(3)]).expect("enqueue");
        queue.enqueue(Vec::new()).expect("empty batch is a no-op");

        wait_for_spans(&store, 3).await;

        let stats = queue.stats();
        assert_eq!(stats.accepted_total, 3);
        assert_eq!(stats.dropped_total, 0);
        assert_eq!(stats.failed_total, 0);
        assert_eq!(stats.queue_capacity, 16);
        assert!(stats.last_ingest_ms.is_some());
    }

    #[tokio::test]
    async fn full_queue_is_reported_not_awaited() {
        let dir = TempDir::new().expect("temp dir");
        let store = SpanStore::open(dir.path()).await.expect("open");
        // A single-thread runtime never yields to the writer inside this
        // test body, so the channel fills deterministically.
        let queue = IngestQueue::start_with(store, 2, Duration::from_secs(60));

        queue.enqueue(vec![row(1)]).expect("first fits");
        queue.enqueue(vec![row(2)]).expect("second fits");
        let err = queue
            .enqueue(vec![row(3), row(4)])
            .expect_err("third overflows");
        assert_eq!(err.capacity, 2);

        let stats = queue.stats();
        assert_eq!(stats.accepted_total, 2);
        assert_eq!(stats.dropped_total, 2);
        assert_eq!(stats.queue_depth, 2);
    }
}
