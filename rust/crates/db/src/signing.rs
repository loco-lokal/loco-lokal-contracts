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
        r#"INSERT INTO submitters (submission_id, template_id, account_id, uuid, email, phone, slug,
                                   values, preferences, metadata, created_at, updated_at)
           VALUES ($1, (SELECT template_id FROM submissions WHERE id = $1), $2, $3, $4, $5, $6,
                   '{}', '{}', '{}', (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
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

/// A `submitter_versions` row: the contact details a submitter had before a change.
#[derive(Clone, Debug)]
pub struct SubmitterVersion {
    pub submitter_id: i64,
    pub created_at: NaiveDateTime,
    pub name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

/// The submitters' versions, oldest first (`submitter_versions.sort_by(&:created_at)`).
pub async fn submitter_versions(
    db: impl PgExecutor<'_>,
    submitter_ids: &[i64],
) -> Result<Vec<SubmitterVersion>, DbError> {
    Ok(sqlx::query_as!(
        SubmitterVersion,
        r#"SELECT submitter_id, created_at, name, email, phone FROM submitter_versions
           WHERE submitter_id = ANY($1) ORDER BY created_at, id"#,
        submitter_ids
    )
    .fetch_all(db)
    .await?)
}

/// `Submitters::StartForm.build_submitters_scope(template, exclude_completed:)
/// .find_by(find_params)`: the newest open submitter of a template's active, non-expired
/// submissions with these contact details. A `None` detail is not filtered on.
pub async fn find_start_form_submitter(
    db: impl PgExecutor<'_>,
    template_id: i64,
    email: Option<&str>,
    phone: Option<&str>,
    exclude_completed: bool,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                  r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                  r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                  r.preferences AS "preferences: JsonText",
                  r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                  r.timezone, r.template_id
           FROM submitters r
           WHERE r.template_id = $1
             AND r.submission_id IN (
               SELECT s.id FROM submissions s
               WHERE s.template_id = $1 AND s.archived_at IS NULL
                 AND (s.expire_at IS NULL OR s.expire_at >= (now() AT TIME ZONE 'utc')))
             AND r.declined_at IS NULL AND r.external_id IS NULL
             AND ($4 = false OR r.completed_at IS NULL)
             AND ($2::varchar IS NULL OR r.email = $2)
             AND ($3::varchar IS NULL OR r.phone = $3)
           ORDER BY r.id DESC LIMIT 1"#,
        template_id,
        email,
        phone,
        exclude_completed,
    )
    .fetch_optional(db)
    .await?)
}

/// `StartFormSelfController#update`: the user's latest open submitter of the template's
/// `self` submissions (`build_submitters_scope(exclude_completed: true, source: :self)`).
pub async fn find_self_start_form_submitter(
    db: impl PgExecutor<'_>,
    template_id: i64,
    email: &str,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                  r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                  r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                  r.preferences AS "preferences: JsonText",
                  r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                  r.timezone, r.template_id
           FROM submitters r
           WHERE r.template_id = $1
             AND r.submission_id IN (
               SELECT s.id FROM submissions s
               WHERE s.template_id = $1 AND s.archived_at IS NULL AND s.source = 'self'
                 AND (s.expire_at IS NULL OR s.expire_at >= (now() AT TIME ZONE 'utc')))
             AND r.declined_at IS NULL AND r.external_id IS NULL AND r.completed_at IS NULL
             AND r.email = $2
           ORDER BY r.id DESC LIMIT 1"#,
        template_id,
        email,
    )
    .fetch_optional(db)
    .await?)
}

/// `StartFormResubmitController#find_or_initialize_submitter`: the open submitter with the
/// slug of the `start_form_slug` cookie and the resubmitted party's email.
pub async fn find_start_form_submitter_by_slug(
    db: impl PgExecutor<'_>,
    template_id: i64,
    slug: Option<&str>,
    email: Option<&str>,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                  r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                  r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                  r.preferences AS "preferences: JsonText",
                  r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                  r.timezone, r.template_id
           FROM submitters r
           WHERE r.template_id = $1
             AND r.submission_id IN (
               SELECT s.id FROM submissions s
               WHERE s.template_id = $1 AND s.archived_at IS NULL
                 AND (s.expire_at IS NULL OR s.expire_at >= (now() AT TIME ZONE 'utc')))
             AND r.declined_at IS NULL AND r.external_id IS NULL AND r.completed_at IS NULL
             AND r.slug IS NOT DISTINCT FROM $2 AND r.email IS NOT DISTINCT FROM $3
           ORDER BY r.id DESC LIMIT 1"#,
        template_id,
        slug,
        email,
    )
    .fetch_optional(db)
    .await?)
}

/// `StartFormController#completed`: a completed submitter of the template's submissions
/// with these contact details (`find_by!`, unordered).
pub async fn find_completed_start_form_submitter(
    db: impl PgExecutor<'_>,
    template_id: i64,
    email: Option<&str>,
    phone: Option<&str>,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                  r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                  r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                  r.preferences AS "preferences: JsonText",
                  r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                  r.timezone, r.template_id
           FROM submitters r
           WHERE r.submission_id IN (SELECT s.id FROM submissions s WHERE s.template_id = $1)
             AND r.completed_at IS NOT NULL
             AND ($2::varchar IS NULL OR r.email = $2)
             AND ($3::varchar IS NULL OR r.phone = $3)
           LIMIT 1"#,
        template_id,
        email,
        phone,
    )
    .fetch_optional(db)
    .await?)
}

/// `submitter.assign_attributes(ip:, ua:)` and `save`: bumps `updated_at` only on a change.
pub async fn set_submitter_request(
    db: impl PgExecutor<'_>,
    id: i64,
    ip: Option<&str>,
    ua: Option<&str>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submitters SET ip = $2, ua = $3,
             updated_at = CASE WHEN (ip, ua) IS NOT DISTINCT FROM ($2::varchar, $3::varchar)
                               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1"#,
        id,
        ip,
        ua,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `EmailEvent.exists?(tag:, email:, emailable:, event_type: :send, created_at: since..)`.
pub async fn email_sent_since(
    db: impl PgExecutor<'_>,
    tag: &str,
    email: &str,
    emailable_type: &str,
    emailable_id: i64,
    since: NaiveDateTime,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM email_events
             WHERE tag = $1 AND email = $2 AND emailable_type = $3 AND emailable_id = $4
               AND event_type = 'send' AND created_at >= $5
               AND created_at <= (now() AT TIME ZONE 'utc')) AS "exists!""#,
        tag,
        email,
        emailable_type,
        emailable_id,
        since,
    )
    .fetch_one(db)
    .await?)
}

/// `Submission.find_by(slug:)`'s id.
pub async fn submission_id_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> Result<Option<i64>, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT id FROM submissions WHERE slug = $1", slug)
            .fetch_optional(db)
            .await?,
    )
}

/// `submitter.submitter_versions.create!(slug:, email:, name:, phone:)`.
pub async fn insert_submitter_version(
    db: impl PgExecutor<'_>,
    submitter: &Submitter,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO submitter_versions (submitter_id, slug, email, name, phone, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))"#,
        submitter.id,
        submitter.slug,
        submitter.email,
        submitter.name,
        submitter.phone,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `SubmitFormDelegateController#create`: hands the form to another email under a new slug.
pub async fn delegate(
    db: impl PgExecutor<'_>,
    id: i64,
    email: &str,
    slug: &str,
    values: &JsonText,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submitters SET email = $2, phone = NULL, name = NULL, slug = $3, values = $4,
                                 updated_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1"#,
        id,
        email,
        slug,
        values as _,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The newest `event_timestamp` of the submitter's events of this type.
pub async fn latest_event_timestamp(
    db: impl PgExecutor<'_>,
    submitter_id: i64,
    event_type: &str,
) -> Result<Option<NaiveDateTime>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT event_timestamp FROM submission_events
           WHERE submitter_id = $1 AND event_type = $2
           ORDER BY event_timestamp DESC LIMIT 1"#,
        submitter_id,
        event_type,
    )
    .fetch_optional(db)
    .await?)
}

/// `DocumentMetadata.find_by(account_id:, blob_checksum:)&.text_runs` (JSON text).
pub async fn document_metadata_text_runs(
    db: impl PgExecutor<'_>,
    account_id: i64,
    blob_checksum: &str,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT text_runs FROM document_metadata WHERE account_id = $1 AND blob_checksum = $2",
        account_id,
        blob_checksum
    )
    .fetch_optional(db)
    .await?)
}

/// `DocumentMetadata.create!(account_id:, blob_checksum:, text_runs:)`. A concurrent insert
/// of the same document wins (Rails retries the lookup on `RecordNotUnique`).
pub async fn insert_document_metadata(
    db: impl PgExecutor<'_>,
    account_id: i64,
    blob_checksum: &str,
    text_runs: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO document_metadata (account_id, blob_checksum, text_runs, created_at)
           VALUES ($1, $2, $3, (now() AT TIME ZONE 'utc'))
           ON CONFLICT (account_id, blob_checksum) DO NOTHING"#,
        account_id,
        blob_checksum,
        text_runs
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `record.touch` after an attachment is added (`touch_attachment_records`).
pub async fn touch_submitter(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE submitters SET updated_at = (clock_timestamp() AT TIME ZONE 'utc') WHERE id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}
