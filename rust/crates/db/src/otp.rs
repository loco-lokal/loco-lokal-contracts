//! `OtpChallenge` rows (`lib/otp_challenges.rb`): one per key and version, holding the
//! nonce of the one-time code and its verification attempts.

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use crate::DbError;

#[derive(Clone, Debug)]
pub struct OtpChallenge {
    pub id: i64,
    pub attempts: i32,
    pub expire_at: NaiveDateTime,
    pub nonce: String,
    pub verified_at: Option<NaiveDateTime>,
    pub version: i32,
}

/// `OtpChallenge.where(key:).order(version: :desc).first`.
pub async fn latest(db: impl PgExecutor<'_>, key: &str) -> Result<Option<OtpChallenge>, DbError> {
    Ok(sqlx::query_as!(
        OtpChallenge,
        r#"SELECT id, attempts, expire_at, nonce, verified_at, version FROM otp_challenges
           WHERE key = $1 ORDER BY version DESC LIMIT 1"#,
        key
    )
    .fetch_optional(db)
    .await?)
}

/// `OtpChallenge.where(key:, expire_at: since..).order(version: :desc)`.
pub async fn recent(
    db: impl PgExecutor<'_>,
    key: &str,
    since: NaiveDateTime,
) -> Result<Vec<OtpChallenge>, DbError> {
    Ok(sqlx::query_as!(
        OtpChallenge,
        r#"SELECT id, attempts, expire_at, nonce, verified_at, version FROM otp_challenges
           WHERE key = $1 AND expire_at >= $2 ORDER BY version DESC"#,
        key,
        since
    )
    .fetch_all(db)
    .await?)
}

/// `OtpChallenge.where(key:).sum(:attempts)`.
pub async fn total_attempts(db: impl PgExecutor<'_>, key: &str) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(attempts), 0)::bigint AS "sum!" FROM otp_challenges WHERE key = $1"#,
        key
    )
    .fetch_one(db)
    .await?)
}

/// `create_with(nonce:, expire_at:).create_or_find_by!(key:, version:)`: the nonce of the
/// stored row.
pub async fn create_or_find(
    db: impl PgExecutor<'_> + Copy,
    key: &str,
    version: i32,
    nonce: &str,
    expire_at: NaiveDateTime,
) -> Result<String, DbError> {
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO otp_challenges (key, version, nonce, expire_at, attempts, created_at)
           VALUES ($1, $2, $3, $4, 0, (now() AT TIME ZONE 'utc'))
           ON CONFLICT (key, version) DO NOTHING RETURNING nonce"#,
        key,
        version,
        nonce,
        expire_at
    )
    .fetch_optional(db)
    .await?;
    if let Some(nonce) = inserted {
        return Ok(nonce);
    }
    Ok(sqlx::query_scalar!(
        "SELECT nonce FROM otp_challenges WHERE key = $1 AND version = $2",
        key,
        version
    )
    .fetch_one(db)
    .await?)
}

/// `OtpChallenge.where(id:, verified_at: nil).update_all(verified_at: Time.current) == 1`.
pub async fn mark_verified(db: impl PgExecutor<'_>, id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"UPDATE otp_challenges SET verified_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1 AND verified_at IS NULL"#,
        id
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// `increment!(:attempts)`.
pub async fn increment_attempts(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE otp_challenges SET attempts = COALESCE(attempts, 0) + 1 WHERE id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}
