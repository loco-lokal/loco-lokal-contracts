//! `users` (Devise: database authenticatable, recoverable, rememberable, trackable,
//! lockable, two-factor) and `accounts`.

use chrono::NaiveDateTime;
use sqlx::{PgExecutor, PgPool};

use crate::DbError;

/// `User::ADMIN_ROLE`, the default (and only) role.
pub const ADMIN_ROLE: &str = "admin";

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub uuid: String,
    pub account_id: i64,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: String,
    pub role: String,
    pub encrypted_password: String,
    pub reset_password_token: Option<String>,
    pub reset_password_sent_at: Option<NaiveDateTime>,
    pub sign_in_count: i32,
    pub current_sign_in_at: Option<NaiveDateTime>,
    pub last_sign_in_at: Option<NaiveDateTime>,
    pub current_sign_in_ip: Option<String>,
    pub last_sign_in_ip: Option<String>,
    pub failed_attempts: i32,
    pub locked_at: Option<NaiveDateTime>,
    pub archived_at: Option<NaiveDateTime>,
    /// Active Record–encrypted Base32 secret.
    pub otp_secret: Option<String>,
    pub consumed_timestep: Option<i32>,
    pub otp_required_for_login: bool,
    pub confirmed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

impl User {
    /// `User#initials`.
    #[must_use]
    pub fn initials(&self) -> String {
        [&self.first_name, &self.last_name]
            .into_iter()
            .filter_map(|n| n.as_deref().and_then(|n| n.chars().next()))
            .collect::<String>()
            .to_uppercase()
    }

    /// `User#full_name`.
    #[must_use]
    pub fn full_name(&self) -> String {
        [&self.first_name, &self.last_name]
            .into_iter()
            .filter_map(|n| n.as_deref().filter(|n| !n.trim().is_empty()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `User#friendly_name`, used as the email recipient.
    #[must_use]
    pub fn friendly_name(&self) -> String {
        let name = self.full_name();
        if name.is_empty() {
            self.email.clone()
        } else {
            format!("\"{}\" <{}>", name.replace('"', ""), self.email)
        }
    }

    /// Devise's `authenticatable_salt`: stored with the session so that changing the
    /// password signs out every session.
    #[must_use]
    pub fn authenticatable_salt(&self) -> &str {
        self.encrypted_password
            .get(..30)
            .unwrap_or(&self.encrypted_password)
    }
}

macro_rules! select_user {
    ($where:literal, $($arg:expr),*) => {
        sqlx::query_as!(
            User,
            "SELECT id, uuid, account_id, first_name, last_name, email, role, encrypted_password,
                    reset_password_token, reset_password_sent_at, sign_in_count,
                    current_sign_in_at, last_sign_in_at, current_sign_in_ip, last_sign_in_ip,
                    failed_attempts, locked_at, archived_at, otp_secret, consumed_timestep,
                    otp_required_for_login, confirmed_at, created_at, updated_at
             FROM users WHERE " + $where,
            $($arg),*
        )
    };
}

pub async fn any_exist(db: &PgPool) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM users) AS "exists!""#)
            .fetch_one(db)
            .await?,
    )
}

pub async fn find(db: impl PgExecutor<'_>, id: i64) -> Result<Option<User>, DbError> {
    Ok(select_user!("id = $1", id).fetch_optional(db).await?)
}

pub async fn find_by_uuid(db: impl PgExecutor<'_>, uuid: &str) -> Result<Option<User>, DbError> {
    Ok(select_user!("uuid = $1", uuid).fetch_optional(db).await?)
}

/// Devise stores emails stripped and downcased, so this is an exact match on the
/// normalized address.
pub async fn find_by_email(db: impl PgExecutor<'_>, email: &str) -> Result<Option<User>, DbError> {
    Ok(select_user!("email = $1", normalize_email(email))
        .fetch_optional(db)
        .await?)
}

pub async fn find_by_reset_password_token(
    db: impl PgExecutor<'_>,
    digest: &str,
) -> Result<Option<User>, DbError> {
    Ok(select_user!("reset_password_token = $1", digest)
        .fetch_optional(db)
        .await?)
}

/// The active user an API token (`X-Auth-Token`) belongs to, found by the token's
/// SHA-256 hex digest.
pub async fn find_by_access_token_digest(
    db: impl PgExecutor<'_>,
    sha256: &str,
) -> Result<Option<User>, DbError> {
    Ok(select_user!(
        "archived_at IS NULL AND id = (SELECT user_id FROM access_tokens WHERE sha256 = $1)",
        sha256
    )
    .fetch_optional(db)
    .await?)
}

/// Devise's `case_insensitive_keys` and `strip_whitespace_keys` for `email`.
#[must_use]
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

pub struct NewUser<'a> {
    pub account_id: i64,
    pub first_name: &'a str,
    pub last_name: &'a str,
    pub email: &'a str,
    pub encrypted_password: &'a str,
}

pub async fn insert(db: impl PgExecutor<'_>, user: NewUser<'_>) -> Result<User, DbError> {
    let uuid = uuid::Uuid::new_v4().to_string();
    Ok(sqlx::query_as!(
        User,
        "INSERT INTO users (account_id, first_name, last_name, email, role, encrypted_password,
                            uuid, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')
         RETURNING id, uuid, account_id, first_name, last_name, email, role, encrypted_password,
                   reset_password_token, reset_password_sent_at, sign_in_count,
                   current_sign_in_at, last_sign_in_at, current_sign_in_ip, last_sign_in_ip,
                   failed_attempts, locked_at, archived_at, otp_secret, consumed_timestep,
                   otp_required_for_login, confirmed_at, created_at, updated_at",
        user.account_id,
        user.first_name,
        user.last_name,
        normalize_email(user.email),
        ADMIN_ROLE,
        user.encrypted_password,
        uuid,
    )
    .fetch_one(db)
    .await?)
}

/// Whether another user already has this (normalized) email.
pub async fn email_taken(db: impl PgExecutor<'_>, email: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM users WHERE email = $1) AS "exists!""#,
        normalize_email(email)
    )
    .fetch_one(db)
    .await?)
}

/// Devise trackable (`update_tracked_fields!`) plus lockable's reset of
/// `failed_attempts` after a successful sign-in.
pub async fn record_sign_in(
    db: impl PgExecutor<'_>,
    id: i64,
    ip: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users SET
            last_sign_in_at = COALESCE(current_sign_in_at, now() AT TIME ZONE 'UTC'),
            current_sign_in_at = now() AT TIME ZONE 'UTC',
            last_sign_in_ip = COALESCE(current_sign_in_ip, $2),
            current_sign_in_ip = $2,
            sign_in_count = sign_in_count + 1,
            failed_attempts = 0,
            updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1",
        id,
        ip
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Lockable after a failed attempt: increments `failed_attempts` and sets `locked_at`
/// once it reaches `maximum_attempts` (an existing lock is kept). Returns the new
/// `(failed_attempts, locked_at)`.
pub async fn record_failed_attempt(
    db: impl PgExecutor<'_>,
    id: i64,
    maximum_attempts: i32,
) -> Result<(i32, Option<NaiveDateTime>), DbError> {
    let row = sqlx::query!(
        "UPDATE users SET
            failed_attempts = failed_attempts + 1,
            locked_at = CASE
                WHEN locked_at IS NULL AND failed_attempts + 1 >= $2 THEN now() AT TIME ZONE 'UTC'
                ELSE locked_at
            END,
            updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1
         RETURNING failed_attempts, locked_at",
        id,
        maximum_attempts
    )
    .fetch_one(db)
    .await?;
    Ok((row.failed_attempts, row.locked_at))
}

/// Lockable's `unlock_access!`.
pub async fn unlock(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users SET locked_at = NULL, failed_attempts = 0, unlock_token = NULL,
                          updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Stores the digest of a new reset-password token (`set_reset_password_token`).
pub async fn set_reset_password_token(
    db: impl PgExecutor<'_>,
    id: i64,
    digest: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users SET reset_password_token = $2,
                          reset_password_sent_at = now() AT TIME ZONE 'UTC',
                          updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1",
        id,
        digest
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `reset_password`: sets the new hash and clears the reset token. When `confirm` is
/// set (accepting an invitation), `confirmed_at` is filled in if it was empty.
pub async fn reset_password(
    db: impl PgExecutor<'_>,
    id: i64,
    encrypted_password: &str,
    confirm: bool,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users SET encrypted_password = $2,
                          reset_password_token = NULL,
                          reset_password_sent_at = NULL,
                          confirmed_at = CASE WHEN $3 THEN COALESCE(confirmed_at, now() AT TIME ZONE 'UTC')
                                              ELSE confirmed_at END,
                          updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1",
        id,
        encrypted_password,
        confirm
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Records the OTP timestep just used, so the code cannot be used again
/// (`consume_otp!`). Returns false when another request already consumed it.
pub async fn consume_otp_timestep(
    db: impl PgExecutor<'_>,
    id: i64,
    timestep: i32,
) -> Result<bool, DbError> {
    let updated = sqlx::query!(
        "UPDATE users SET consumed_timestep = $2, updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1 AND (consumed_timestep IS NULL OR consumed_timestep < $2)",
        id,
        timestep
    )
    .execute(db)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Devise `forget_me!` on sign-out (`expire_all_remember_me_on_sign_out`).
pub async fn forget_me(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE users SET remember_created_at = NULL WHERE id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub timezone: String,
    pub locale: String,
    pub archived_at: Option<NaiveDateTime>,
    /// `account.testing?`: the account is the testing account linked to another one.
    pub testing: bool,
    /// `account.linked_account_account.present?`.
    pub linked: bool,
}

pub async fn find_account(db: impl PgExecutor<'_>, id: i64) -> Result<Option<Account>, DbError> {
    Ok(sqlx::query_as!(
        Account,
        r#"SELECT a.id, a.uuid, a.name, a.timezone, a.locale, a.archived_at,
                  COALESCE(l.account_type = 'testing', false) AS "testing!",
                  (l.id IS NOT NULL) AS "linked!"
           FROM accounts a
           LEFT JOIN LATERAL (
               SELECT id, account_type FROM account_linked_accounts
               WHERE linked_account_id = a.id ORDER BY id LIMIT 1
           ) l ON true
           WHERE a.id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?)
}

pub struct NewAccount<'a> {
    pub name: &'a str,
    pub timezone: &'a str,
    pub locale: &'a str,
}

pub async fn insert_account(
    db: impl PgExecutor<'_>,
    account: NewAccount<'_>,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        "INSERT INTO accounts (name, timezone, locale, uuid, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')
         RETURNING id",
        account.name,
        account.timezone,
        account.locale,
        uuid::Uuid::new_v4().to_string()
    )
    .fetch_one(db)
    .await?)
}

/// Inserts an `encrypted_configs` row; `value` must already be encrypted.
pub async fn insert_encrypted_config(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
    encrypted_value: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO encrypted_configs (account_id, key, value, created_at, updated_at)
         VALUES ($1, $2, $3, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')",
        account_id,
        key,
        encrypted_value
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Inserts an `account_configs` row; `value` is serialized as JSON, as Rails does.
pub async fn insert_account_config(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO account_configs (account_id, key, value, created_at, updated_at)
         VALUES ($1, $2, $3, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')",
        account_id,
        key,
        value.to_string()
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The account's encrypted config value for `key`, still encrypted.
pub async fn encrypted_config_value(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT value FROM encrypted_configs WHERE account_id = $1 AND key = $2",
        account_id,
        key
    )
    .fetch_optional(db)
    .await?)
}

/// `AccountConfig.find_by(account:, key:)&.value`, parsed from its JSON text.
pub async fn account_config(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    let value = sqlx::query_scalar!(
        "SELECT value FROM account_configs WHERE account_id = $1 AND key = $2",
        account_id,
        key
    )
    .fetch_optional(db)
    .await?;
    Ok(value.map(|v| serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)))
}

/// The user columns the API includes (`author`, `created_by_user`).
#[derive(Clone, Debug)]
pub struct UserBrief {
    pub id: i64,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

pub async fn find_briefs(db: impl PgExecutor<'_>, ids: &[i64]) -> Result<Vec<UserBrief>, DbError> {
    Ok(sqlx::query_as!(
        UserBrief,
        "SELECT id, email, first_name, last_name FROM users WHERE id = ANY($1)",
        ids
    )
    .fetch_all(db)
    .await?)
}

/// An account's user by email (`account.users.find_by(email:)`).
pub async fn find_id_in_account_by_email(
    db: impl PgExecutor<'_>,
    account_id: i64,
    email: &str,
) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT id FROM users WHERE account_id = $1 AND email = $2 ORDER BY id LIMIT 1",
        account_id,
        email
    )
    .fetch_optional(db)
    .await?)
}

/// Whether `record_account_id` is the other side of the current account's testing link:
/// its production account when `current_is_testing`, else one of its testing accounts.
pub async fn is_linked_testing_counterpart(
    db: impl PgExecutor<'_>,
    current_account_id: i64,
    current_is_testing: bool,
    record_account_id: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM account_linked_accounts
             WHERE account_type = 'testing'
               AND ((NOT $3 AND account_id = $1 AND linked_account_id = $2)
                 OR ($3 AND linked_account_id = $1 AND account_id = $2))
           ) AS "exists!""#,
        current_account_id,
        record_account_id,
        current_is_testing,
    )
    .fetch_one(db)
    .await?)
}

/// `account.linked_account_account&.account_id`: the production account a testing (or
/// otherwise linked) account belongs to.
pub async fn linked_parent_account_id(
    db: impl PgExecutor<'_>,
    account_id: i64,
) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT account_id FROM account_linked_accounts
         WHERE linked_account_id = $1 ORDER BY id LIMIT 1",
        account_id
    )
    .fetch_optional(db)
    .await?)
}
