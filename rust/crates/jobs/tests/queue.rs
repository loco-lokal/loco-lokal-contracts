// Test jobs implement the async trait without awaiting anything.
#![allow(clippy::unused_async_trait_impl)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use loco_jobs::{EnqueueOptions, Job, Registry, WorkerConfig, enqueue};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Ctx {
    runs: Arc<AtomicUsize>,
}

#[derive(Serialize, Deserialize)]
struct Succeeds {
    n: usize,
}

impl Job for Succeeds {
    const KIND: &'static str = "test/succeeds";
    type Context = Ctx;

    async fn run(self, ctx: Ctx) -> anyhow::Result<()> {
        ctx.runs.fetch_add(self.n, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Fails;

impl Job for Fails {
    const KIND: &'static str = "test/fails";
    const MAX_ATTEMPTS: i32 = 2;
    type Context = Ctx;

    async fn run(self, _ctx: Ctx) -> anyhow::Result<()> {
        anyhow::bail!("boom")
    }
}

fn registry() -> Registry<Ctx> {
    Registry::default()
        .register::<Succeeds>()
        .register::<Fails>()
}

#[sqlx::test(migrator = "loco_db::MIGRATOR")]
async fn runs_and_deletes_successful_jobs(db: PgPool) {
    let ctx = Ctx::default();
    enqueue(&db, &Succeeds { n: 3 }, EnqueueOptions::default())
        .await
        .unwrap();

    let config = WorkerConfig::default();
    assert!(registry().work_one(&db, &ctx, &config, "t").await.unwrap());
    assert!(!registry().work_one(&db, &ctx, &config, "t").await.unwrap());

    assert_eq!(ctx.runs.load(Ordering::SeqCst), 3);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM rust_jobs")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "loco_db::MIGRATOR")]
async fn retries_then_marks_failed(db: PgPool) {
    let ctx = Ctx::default();
    let id = enqueue(&db, &Fails, EnqueueOptions::default())
        .await
        .unwrap();
    let config = WorkerConfig::default();

    assert!(registry().work_one(&db, &ctx, &config, "t").await.unwrap());
    // Retry is scheduled in the future, so nothing is ready now.
    assert!(!registry().work_one(&db, &ctx, &config, "t").await.unwrap());

    sqlx::query("UPDATE rust_jobs SET run_at = now() WHERE id = $1")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    assert!(registry().work_one(&db, &ctx, &config, "t").await.unwrap());

    let (attempts, failed, error): (i32, bool, Option<String>) = sqlx::query_as(
        "SELECT attempts, failed_at IS NOT NULL, last_error FROM rust_jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();

    assert_eq!(
        (attempts, failed, error.as_deref()),
        (2, true, Some("boom"))
    );
}

#[sqlx::test(migrator = "loco_db::MIGRATOR")]
async fn enqueue_in_rolled_back_transaction_is_discarded(db: PgPool) {
    let mut tx = db.begin().await.unwrap();
    enqueue(&mut *tx, &Succeeds { n: 1 }, EnqueueOptions::default())
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    let ctx = Ctx::default();
    assert!(
        !registry()
            .work_one(&db, &ctx, &WorkerConfig::default(), "t")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "loco_db::MIGRATOR")]
async fn workers_drain_queue_and_stop_on_shutdown(db: PgPool) {
    let ctx = Ctx::default();
    for _ in 0..20 {
        enqueue(&db, &Succeeds { n: 1 }, EnqueueOptions::default())
            .await
            .unwrap();
    }

    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = tokio::spawn(registry().run(
        db.clone(),
        ctx.clone(),
        WorkerConfig {
            concurrency: 4,
            ..WorkerConfig::default()
        },
        shutdown.clone(),
    ));

    for _ in 0..100 {
        if ctx.runs.load(Ordering::SeqCst) == 20 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    handle.await.unwrap();

    assert_eq!(ctx.runs.load(Ordering::SeqCst), 20);
}
