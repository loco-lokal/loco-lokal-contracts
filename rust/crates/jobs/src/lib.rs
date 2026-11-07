//! Postgres-backed background jobs, replacing Sidekiq and Redis.
//!
//! - Jobs live in `rust_jobs` (created by `rust/migrations`, see `loco_db::migrate`) and are claimed with `FOR UPDATE SKIP LOCKED`, so any number
//!   of workers and processes can share a queue.
//! - [`enqueue`] takes any executor, so a job can be enqueued inside the transaction that
//!   creates the data it works on. It runs only if that transaction commits.
//! - Failed jobs are retried with Sidekiq's backoff (`attempts⁴ + 15 + rand(10)·(attempts+1)`
//!   seconds) until `max_attempts`, then kept with `failed_at` set for inspection.
//! - Jobs locked longer than the lock timeout (a crashed worker) become available again.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use rand::RngExt as _;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, postgres::PgListener};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const NOTIFY_CHANNEL: &str = "rust_jobs";

/// A job type: its payload is the struct itself, serialized as JSON.
pub trait Job: Serialize + DeserializeOwned + Send + 'static {
    /// Stable identifier stored in `rust_jobs.kind`.
    const KIND: &'static str;
    const QUEUE: &'static str = "default";
    const MAX_ATTEMPTS: i32 = 25;

    type Context: Clone + Send + Sync + 'static;

    fn run(self, ctx: Self::Context) -> impl Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug, Default, Clone)]
pub struct EnqueueOptions {
    pub run_at: Option<DateTime<Utc>>,
}

/// Enqueues `job`; returns the job id.
pub async fn enqueue<'e, J: Job>(
    executor: impl PgExecutor<'e>,
    job: &J,
    options: EnqueueOptions,
) -> Result<i64, sqlx::Error> {
    let payload = serde_json::to_value(job).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;

    sqlx::query_scalar!(
        r#"
        WITH inserted AS (
          INSERT INTO rust_jobs (queue, kind, payload, max_attempts, run_at)
          VALUES ($1, $2, $3, $4, COALESCE($5, now()))
          RETURNING id, queue
        )
        SELECT id AS "id!" FROM inserted, LATERAL (SELECT pg_notify($6, inserted.queue)) n
        "#,
        J::QUEUE,
        J::KIND,
        payload,
        J::MAX_ATTEMPTS,
        options.run_at,
        NOTIFY_CHANNEL,
    )
    .fetch_one(executor)
    .await
}

type Handler<C> =
    Arc<dyn Fn(Value, C) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub queues: Vec<String>,
    pub concurrency: usize,
    pub poll_interval: Duration,
    pub lock_timeout: Duration,
    pub worker_id: String,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            queues: vec!["default".into()],
            concurrency: 5,
            poll_interval: Duration::from_secs(1),
            lock_timeout: Duration::from_mins(30),
            worker_id: format!("{}:{}", hostname(), std::process::id()),
        }
    }
}

/// Registered job handlers sharing one context type.
pub struct Registry<C> {
    handlers: HashMap<&'static str, Handler<C>>,
}

impl<C: Clone + Send + Sync + 'static> Default for Registry<C> {
    fn default() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }
}

impl<C: Clone + Send + Sync + 'static> Registry<C> {
    #[must_use]
    pub fn register<J: Job<Context = C>>(mut self) -> Self {
        self.handlers.insert(
            J::KIND,
            Arc::new(|payload, ctx| {
                Box::pin(async move {
                    let job: J = serde_json::from_value(payload)?;
                    job.run(ctx).await
                })
            }),
        );
        self
    }

    /// Runs workers until `shutdown` is cancelled, then waits for running jobs to finish.
    pub async fn run(self, db: PgPool, ctx: C, config: WorkerConfig, shutdown: CancellationToken) {
        let registry = Arc::new(self);
        let wake = Arc::new(tokio::sync::Notify::new());
        let tracker = TaskTracker::new();

        tracker.spawn(listen(db.clone(), wake.clone(), shutdown.clone()));

        for slot in 0..config.concurrency {
            let (registry, db, ctx, config, wake, shutdown) = (
                registry.clone(),
                db.clone(),
                ctx.clone(),
                config.clone(),
                wake.clone(),
                shutdown.clone(),
            );
            tracker.spawn(async move {
                let worker_id = format!("{}#{slot}", config.worker_id);
                while !shutdown.is_cancelled() {
                    match registry.work_one(&db, &ctx, &config, &worker_id).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tokio::select! {
                                () = wake.notified() => {}
                                () = tokio::time::sleep(config.poll_interval) => {}
                                () = shutdown.cancelled() => {}
                            }
                        }
                        Err(err) => {
                            tracing::error!(%err, "job worker database error");
                            tokio::time::sleep(config.poll_interval).await;
                        }
                    }
                }
            });
        }

        tracker.close();
        tracker.wait().await;
    }

    /// Claims and runs one job. Returns whether a job was found.
    pub async fn work_one(
        &self,
        db: &PgPool,
        ctx: &C,
        config: &WorkerConfig,
        worker_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let lock_timeout_secs = config.lock_timeout.as_secs_f64();

        let Some(job) = sqlx::query!(
            r#"
            UPDATE rust_jobs
            SET locked_at = now(), locked_by = $1, attempts = attempts + 1
            WHERE id = (
              SELECT id FROM rust_jobs
              WHERE queue = ANY($2) AND failed_at IS NULL AND run_at <= now()
                AND (locked_at IS NULL OR locked_at < now() - make_interval(secs => $3))
              ORDER BY run_at, id
              FOR UPDATE SKIP LOCKED
              LIMIT 1
            )
            RETURNING id, kind, payload, attempts, max_attempts
            "#,
            worker_id,
            &config.queues,
            lock_timeout_secs,
        )
        .fetch_optional(db)
        .await?
        else {
            return Ok(false);
        };

        let outcome = match self.handlers.get(job.kind.as_str()) {
            Some(handler) => {
                let fut = handler(job.payload, ctx.clone());
                match tokio::spawn(fut).await {
                    Ok(result) => result,
                    Err(join_error) => Err(anyhow::anyhow!("job panicked: {join_error}")),
                }
            }
            None => Err(anyhow::anyhow!(
                "no handler registered for job kind {}",
                job.kind
            )),
        };

        match outcome {
            Ok(()) => {
                sqlx::query!("DELETE FROM rust_jobs WHERE id = $1", job.id)
                    .execute(db)
                    .await?;
            }
            Err(err) => {
                let message = format!("{err:#}");
                if job.attempts >= job.max_attempts {
                    tracing::error!(
                        id = job.id,
                        kind = job.kind,
                        error = message,
                        "job failed permanently"
                    );
                    sqlx::query!(
                        "UPDATE rust_jobs SET locked_at = NULL, locked_by = NULL, failed_at = now(), last_error = $2 WHERE id = $1",
                        job.id,
                        message
                    )
                    .execute(db)
                    .await?;
                } else {
                    let delay = retry_delay(job.attempts);
                    tracing::warn!(
                        id = job.id,
                        kind = job.kind,
                        attempt = job.attempts,
                        error = message,
                        retry_in_secs = delay,
                        "job failed; retrying"
                    );
                    sqlx::query!(
                        "UPDATE rust_jobs SET locked_at = NULL, locked_by = NULL, last_error = $2, run_at = now() + make_interval(secs => $3) WHERE id = $1",
                        job.id,
                        message,
                        delay
                    )
                    .execute(db)
                    .await?;
                }
            }
        }

        Ok(true)
    }
}

/// Sidekiq's default retry delay in seconds after `attempts` failures.
#[must_use]
pub fn retry_delay(attempts: i32) -> f64 {
    let count = f64::from(attempts.max(1) - 1);
    let jitter = f64::from(rand::rng().random_range(0..10)) * (count + 1.0);
    count.powi(4) + 15.0 + jitter
}

async fn listen(db: PgPool, wake: Arc<tokio::sync::Notify>, shutdown: CancellationToken) {
    let mut listener = match PgListener::connect_with(&db).await {
        Ok(mut l) => match l.listen(NOTIFY_CHANNEL).await {
            Ok(()) => l,
            Err(err) => {
                tracing::warn!(%err, "job LISTEN failed; falling back to polling");
                return;
            }
        },
        Err(err) => {
            tracing::warn!(%err, "job LISTEN connection failed; falling back to polling");
            return;
        }
    };

    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            msg = listener.recv() => match msg {
                Ok(_) => wake.notify_waiters(),
                Err(err) => {
                    tracing::warn!(%err, "job LISTEN error");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            },
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into())
}
