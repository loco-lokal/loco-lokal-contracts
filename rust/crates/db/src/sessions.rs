//! `rust_sessions`: server-side browser sessions. The cookie holds a random token and
//! the table stores its SHA-256 digest, the signed-in user, and the session data
//! (CSRF secret, flash, return path, impersonation).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::DbError;

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id_digest: String,
    pub user_id: Option<i64>,
    pub auth_salt: Option<String>,
    pub data: serde_json::Value,
    pub expires_at: DateTime<Utc>,
}

pub async fn find(db: impl PgExecutor<'_>, id_digest: &str) -> Result<Option<SessionRow>, DbError> {
    Ok(sqlx::query_as!(
        SessionRow,
        "SELECT id_digest, user_id, auth_salt, data, expires_at
         FROM rust_sessions WHERE id_digest = $1 AND expires_at > now()",
        id_digest
    )
    .fetch_optional(db)
    .await?)
}

pub async fn upsert(db: impl PgExecutor<'_>, row: &SessionRow) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO rust_sessions (id_digest, user_id, auth_salt, data, expires_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id_digest) DO UPDATE SET
            user_id = EXCLUDED.user_id,
            auth_salt = EXCLUDED.auth_salt,
            data = EXCLUDED.data,
            expires_at = EXCLUDED.expires_at,
            updated_at = now()",
        row.id_digest,
        row.user_id,
        row.auth_salt,
        row.data,
        row.expires_at
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: impl PgExecutor<'_>, id_digest: &str) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM rust_sessions WHERE id_digest = $1", id_digest)
        .execute(db)
        .await?;
    Ok(())
}

/// Ends every session of a user (sign-out with `expire_all_remember_me_on_sign_out`).
pub async fn delete_for_user(db: impl PgExecutor<'_>, user_id: i64) -> Result<u64, DbError> {
    Ok(
        sqlx::query!("DELETE FROM rust_sessions WHERE user_id = $1", user_id)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

pub async fn delete_expired(db: impl PgExecutor<'_>) -> Result<u64, DbError> {
    Ok(
        sqlx::query!("DELETE FROM rust_sessions WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
