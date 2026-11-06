//! Database access over the existing Rails-managed PostgreSQL schema (`db/schema.rb`).
//!
//! Until cutover, Rails migrations own the schema. Queries use `sqlx` compile-time
//! checking against a migrated database, or the offline data in `rust/.sqlx/` when
//! `SQLX_OFFLINE=true`.

use std::time::Duration;

pub use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Latest Rails migration version this code was written against (`db/schema.rb`).
#[allow(clippy::inconsistent_digit_grouping)] // grouped like the Rails version YYYY_MM_DD_HHMMSS
pub const EXPECTED_SCHEMA_VERSION: i64 = 2026_10_25_120_002;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;

    Ok(pool)
}

/// Cheap connectivity check used by the `/up` health endpoint.
pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query("SELECT 1").execute(pool).await?;

    Ok(())
}

/// Highest applied Rails migration, or `None` if the database has not been migrated.
pub async fn schema_version(pool: &PgPool) -> Result<Option<i64>, DbError> {
    let version =
        sqlx::query_scalar!(r#"SELECT MAX(version::bigint) AS "version" FROM schema_migrations"#)
            .fetch_one(pool)
            .await?;

    Ok(version)
}

/// Number of users; first-run setup is required when this is zero.
pub async fn users_count(pool: &PgPool) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM users"#)
        .fetch_one(pool)
        .await?;

    Ok(count)
}
