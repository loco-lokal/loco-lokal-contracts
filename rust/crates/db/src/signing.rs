//! Queries of the signing form (`SubmitFormController`, `Submitters::SubmitValues`) and the
//! completion job (`ProcessSubmitterCompletionJob`).

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use crate::{DbError, JsonText, submissions::Submitter};

/// The account's `account_configs` values for `keys` (`AccountConfig#value`, JSON).
pub async fn account_configs(
    db: impl PgExecutor<'_>,
    account_id: i64,
    keys: &[&str],
) -> Result<Vec<(String, serde_json::Value)>, DbError> {
    let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
    let rows = sqlx::query!(
        "SELECT key, value FROM account_configs WHERE account_id = $1 AND key = ANY($2) ORDER BY id",
        account_id,
        &keys
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let value = serde_json::from_str(&r.value)
                .ok()
                .unwrap_or(serde_json::Value::Null);
            (r.key, value)
        })
        .collect())
}

/// The user config `key` (`UserConfig#value`), if set.
pub async fn user_config(
    db: impl PgExecutor<'_>,
    user_id: i64,
    key: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    let value: Option<String> = sqlx::query_scalar!(
        "SELECT value FROM user_configs WHERE user_id = $1 AND key = $2 LIMIT 1",
        user_id,
        key
    )
    .fetch_optional(db)
    .await?;
    Ok(value.map(|v| serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)))
}

/// Whether the submitter has an event of one of `types`.
pub async fn has_event(
    db: impl PgExecutor<'_>,
    submitter_id: i64,
    types: &[&str],
) -> Result<bool, DbError> {
    let types: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM submission_events
                          WHERE submitter_id = $1 AND event_type = ANY($2)) AS "exists!""#,
        submitter_id,
        &types
    )
    .fetch_one(db)
    .await?)
}

/// What `SubmitValues.update_submitter!` saves.
#[derive(Clone, Debug)]
pub struct SubmitterProgress {
    pub values: JsonText,
    pub opened_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub ip: Option<String>,
    pub ua: Option<String>,
    pub timezone: Option<String>,
}

/// `submitter.save!` after the values were merged; `updated_at` moves on a change only.
pub async fn save_progress(
    db: impl PgExecutor<'_>,
    id: i64,
    progress: &SubmitterProgress,
) -> Result<Submitter, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"UPDATE submitters SET
             values = $2, opened_at = $3, completed_at = $4, ip = $5, ua = $6, timezone = $7,
             updated_at = CASE
               WHEN (values, opened_at, completed_at, ip, ua, timezone)
                    IS NOT DISTINCT FROM
                    ($2::text, $3::timestamp, $4::timestamp, $5::varchar, $6::varchar,
                     $7::varchar)
               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1
           RETURNING id, submission_id, uuid, email, slug, values AS "values: JsonText", ua, ip,
                     sent_at, opened_at, completed_at, created_at, updated_at, name, phone,
                     external_id, preferences AS "preferences: JsonText",
                     metadata AS "metadata: JsonText", account_id, declined_at, timezone,
                     template_id"#,
        id,
        progress.values as _,
        progress.opened_at,
        progress.completed_at,
        progress.ip,
        progress.ua,
        progress.timezone,
    )
    .fetch_one(db)
    .await?)
}

/// `submitter.update!(declined_at:, values:)`.
pub async fn decline(db: impl PgExecutor<'_>, id: i64, values: &JsonText) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submitters SET declined_at = (now() AT TIME ZONE 'utc'), values = $2,
                                updated_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1"#,
        id,
        values as _
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `submitter.opened_at = Time.current; submitter.save`.
pub async fn touch_opened(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submitters SET opened_at = (now() AT TIME ZONE 'utc'),
                                updated_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1"#,
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `submission.update!(submitters_order:)`.
pub async fn set_submitters_order(
    db: impl PgExecutor<'_>,
    id: i64,
    order: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submissions SET submitters_order = $2::varchar,
                                 updated_at = CASE WHEN submitters_order = $2::varchar THEN updated_at
                                                   ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1"#,
        id,
        order
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `submission.save!` of the `template_fields` snapshot (signature reasons).
pub async fn save_template_fields(
    db: impl PgExecutor<'_>,
    id: i64,
    fields: &JsonText,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submissions SET template_fields = $2,
                                 updated_at = CASE WHEN template_fields IS NOT DISTINCT FROM $2::text
                                                   THEN updated_at
                                                   ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1"#,
        id,
        fields as _
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `Submissions.maybe_update_completed_at`: marks the submission completed when every
/// non-viewer submitter has completed. Returns whether this call did it.
pub async fn maybe_update_completed_at(
    db: impl PgExecutor<'_>,
    submission_id: i64,
    viewer_uuids: &[String],
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"UPDATE submissions SET completed_at =
             (SELECT MAX(completed_at) FROM submitters WHERE submission_id = submissions.id)
           WHERE id = $1 AND completed_at IS NULL
             AND NOT EXISTS (SELECT 1 FROM submitters
                             WHERE submission_id = $1 AND completed_at IS NULL
                               AND NOT (uuid = ANY($2)))"#,
        submission_id,
        viewer_uuids
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// `submission.submitters.create!(uuid:, email:, phone:, account_id:)` for invites.
pub async fn insert_invited_submitter(
    db: impl PgExecutor<'_>,
    submission_id: i64,
    account_id: i64,
    uuid: &str,
    email: Option<&str>,
    phone: Option<&str>,
    slug: &str,
) -> Result<Submitter, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"INSERT INTO submitters (submission_id, account_id, uuid, email, phone, slug, values,
                                   preferences, metadata, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, '{}', '{}', '{}',
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id, submission_id, uuid, email, slug, values AS "values: JsonText", ua, ip,
                     sent_at, opened_at, completed_at, created_at, updated_at, name, phone,
                     external_id, preferences AS "preferences: JsonText",
                     metadata AS "metadata: JsonText", account_id, declined_at, timezone,
                     template_id"#,
        submission_id,
        account_id,
        uuid,
        email,
        phone,
        slug
    )
    .fetch_one(db)
    .await?)
}

/// A `completed_submitters` row (`ProcessSubmitterCompletionJob#create_completed_submitter!`).
#[derive(Clone, Debug)]
pub struct NewCompletedSubmitter<'a> {
    pub submitter_id: i64,
    pub submission_id: i64,
    pub account_id: i64,
    pub template_id: Option<i64>,
    pub source: &'a str,
    pub sms_count: i32,
    pub verification_method: Option<&'a str>,
    pub completed_at: NaiveDateTime,
}

/// Inserts the row unless the submitter has one; `is_first` marks the submission's first.
pub async fn create_completed_submitter(
    db: impl PgExecutor<'_>,
    new: &NewCompletedSubmitter<'_>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO completed_submitters
             (submitter_id, submission_id, account_id, template_id, source, sms_count,
              verification_method, completed_at, is_first, created_at, updated_at)
           SELECT $1, $2, $3, $4, $5, $6, $7, $8,
                  NOT EXISTS (SELECT 1 FROM completed_submitters
                              WHERE submission_id = $2 AND is_first = true),
                  (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc')
           WHERE NOT EXISTS (SELECT 1 FROM completed_submitters WHERE submitter_id = $1)
           ON CONFLICT DO NOTHING"#,
        new.submitter_id,
        new.submission_id,
        new.account_id,
        new.template_id,
        new.source,
        new.sms_count,
        new.verification_method,
        new.completed_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `CompletedDocument.find_or_create_by!(sha256:, submitter_id:)`.
pub async fn create_completed_document(
    db: impl PgExecutor<'_>,
    submitter_id: i64,
    sha256: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO completed_documents (submitter_id, sha256, created_at, updated_at)
           SELECT $1, $2::varchar, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc')
           WHERE NOT EXISTS (SELECT 1 FROM completed_documents
                             WHERE submitter_id = $1 AND sha256 = $2::varchar)"#,
        submitter_id,
        sha256
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Event `data` of the submitter's events of `types` (`sms_count`, verification method).
pub async fn events_of_types(
    db: impl PgExecutor<'_>,
    submitter_id: i64,
    types: &[&str],
) -> Result<Vec<(String, serde_json::Value)>, DbError> {
    let types: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
    let rows = sqlx::query!(
        r#"SELECT event_type, data AS "data: JsonText" FROM submission_events
           WHERE submitter_id = $1 AND event_type = ANY($2) ORDER BY id"#,
        submitter_id,
        &types
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.event_type, r.data.0)).collect())
}

/// The latest `submitter_versions` row with this slug (`SubmitterVersion.find_by!(slug:)`).
pub async fn submitter_version_submitter_id(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT submitter_id FROM submitter_versions WHERE slug = $1 LIMIT 1",
        slug
    )
    .fetch_optional(db)
    .await?)
}

/// Copies the template's roles when the submission has none (`update_template_fields!`).
pub async fn set_template_submitters(
    db: impl PgExecutor<'_>,
    id: i64,
    submitters: &JsonText,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE submissions SET template_submitters = $2 WHERE id = $1",
        id,
        submitters as _
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The `lock_events` names recorded for `key`, oldest first.
pub async fn lock_event_names(db: impl PgExecutor<'_>, key: &str) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT event_name FROM lock_events WHERE key = $1 ORDER BY id",
        key
    )
    .fetch_all(db)
    .await?)
}

/// `LockEvent.create!(key:, event_name:)`; `false` when a unique `start`/`complete`
/// already exists (`ActiveRecord::RecordNotUnique`).
pub async fn create_lock_event(
    db: impl PgExecutor<'_>,
    key: &str,
    event_name: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"INSERT INTO lock_events (key, event_name, created_at, updated_at)
           VALUES ($1, $2, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))"#,
        key,
        event_name
    )
    .execute(db)
    .await;
    match result {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// `AccountConfig.where(key:).first_or_initialize.value`: the first account config with
/// `key` in any account.
pub async fn first_account_config(
    db: impl PgExecutor<'_>,
    key: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    let value: Option<String> = sqlx::query_scalar!(
        "SELECT value FROM account_configs WHERE key = $1 ORDER BY id LIMIT 1",
        key
    )
    .fetch_optional(db)
    .await?;
    Ok(value.map(|v| serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)))
}

/// `EncryptedConfig.find_by(account:, key:)&.value ||
/// EncryptedConfig.find_by(key:).value`, still encrypted.
pub async fn encrypted_config_with_fallback(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT value FROM encrypted_configs WHERE key = $2
           ORDER BY (account_id = $1) DESC, id LIMIT 1"#,
        account_id,
        key
    )
    .fetch_optional(db)
    .await?)
}
