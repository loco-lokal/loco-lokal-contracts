use loco_server::{AppState, config::Config, job_registry, router};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let db = loco_db::connect(&config.database_url, config.database_pool_size).await?;

    match loco_db::schema_version(&db).await? {
        Some(v) if v == loco_db::EXPECTED_SCHEMA_VERSION => {}
        Some(v) => tracing::warn!(
            found = v,
            expected = loco_db::EXPECTED_SCHEMA_VERSION,
            "database schema version differs from the one this build targets"
        ),
        None => anyhow::bail!("database is not migrated (schema_migrations is empty)"),
    }

    // Rust-owned tables (job queue, sessions); the Rails schema is untouched.
    loco_db::migrate(&db).await?;

    let bind = config.bind;
    let job_concurrency = config.job_concurrency;
    let state = AppState::build(config, db.clone()).await?;
    tracing::info!(
        storage = state.storage.default_service(),
        smtp = matches!(state.mailer.delivery(), loco_mailer::Delivery::Smtp { .. }),
        "configured"
    );

    let shutdown = CancellationToken::new();
    let workers = (job_concurrency > 0).then(|| {
        tokio::spawn(job_registry().run(
            db,
            state.clone(),
            loco_jobs::WorkerConfig {
                concurrency: job_concurrency,
                ..loco_jobs::WorkerConfig::default()
            },
            shutdown.clone(),
        ))
    });

    // Expired sessions are only skipped on lookup; remove them hourly.
    let cleanup_db = state.db.clone();
    let cleanup_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_hours(1));
        loop {
            tokio::select! {
                () = cleanup_shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(err) = loco_db::sessions::delete_expired(&cleanup_db).await {
                        tracing::warn!(%err, "deleting expired sessions failed");
                    }
                }
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "listening");

    let server_shutdown = shutdown.clone();
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        server_shutdown.cancel();
    })
    .await?;

    if let Some(workers) = workers {
        workers.await?;
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
