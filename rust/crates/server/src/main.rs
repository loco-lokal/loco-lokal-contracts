use loco_server::{AppState, config::Config, router};
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

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, "listening");

    axum::serve(listener, router(AppState { db }))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
