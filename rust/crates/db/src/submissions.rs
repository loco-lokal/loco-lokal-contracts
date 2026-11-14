//! `submissions`, `submitters` and `submission_events`.

use chrono::NaiveDateTime;
use sqlx::{PgExecutor, PgPool};

use crate::{DbError, JsonText, templates::Page};

#[derive(Clone, Debug)]
pub struct Submission {
    pub id: i64,
    pub template_id: Option<i64>,
    pub created_by_user_id: Option<i64>,
    pub archived_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub template_fields: Option<JsonText>,
    pub template_schema: Option<JsonText>,
    pub template_submitters: Option<JsonText>,
    pub source: String,
    pub submitters_order: String,
    pub slug: String,
    pub preferences: JsonText,
    pub account_id: i64,
    pub expire_at: Option<NaiveDateTime>,
    pub name: Option<String>,
    pub variables_schema: Option<JsonText>,
    pub variables: Option<JsonText>,
    pub completed_at: Option<NaiveDateTime>,
}

impl Submission {
    /// `Submission#expired?`.
    #[must_use]
    pub fn expired(&self, now: NaiveDateTime) -> bool {
        self.expire_at.is_some_and(|at| at <= now)
    }
}

#[derive(Clone, Debug)]
pub struct Submitter {
    pub id: i64,
    pub submission_id: i64,
    pub uuid: String,
    pub email: Option<String>,
    pub slug: String,
    pub values: JsonText,
    pub ua: Option<String>,
    pub ip: Option<String>,
    pub sent_at: Option<NaiveDateTime>,
    pub opened_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub external_id: Option<String>,
    pub preferences: JsonText,
    pub metadata: JsonText,
    pub account_id: i64,
    pub declined_at: Option<NaiveDateTime>,
    pub timezone: Option<String>,
    pub template_id: Option<i64>,
}

impl Submitter {
    /// `Submitter#status`.
    #[must_use]
    pub fn status(&self) -> &'static str {
        if self.declined_at.is_some() {
            "declined"
        } else if self.completed_at.is_some() {
            "completed"
        } else if self.opened_at.is_some() {
            "opened"
        } else if self.sent_at.is_some() {
            "sent"
        } else {
            "awaiting"
        }
    }

    /// `Submitter#first_name`: the name up to the first whitespace.
    #[must_use]
    pub fn first_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .and_then(|n| n.split_whitespace().next())
    }

    /// `Submitter#friendly_name`, the email recipient.
    #[must_use]
    pub fn friendly_name(&self) -> Option<String> {
        let email = self.email.as_deref()?;
        match self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            Some(name) if !email.contains(',') => Some(format!(
                "\"{}\" <{email}>",
                name.replace('"', "")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
            _ => Some(email.to_owned()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubmissionEvent {
    pub id: i64,
    pub submission_id: i64,
    pub submitter_id: Option<i64>,
    pub data: JsonText,
    pub event_type: String,
    pub event_timestamp: NaiveDateTime,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub account_id: Option<i64>,
}

pub async fn find(db: impl PgExecutor<'_>, id: i64) -> Result<Option<Submission>, DbError> {
    Ok(sqlx::query_as!(
        Submission,
        r#"SELECT s.id, s.template_id, s.created_by_user_id, s.archived_at, s.created_at,
                      s.updated_at, s.template_fields AS "template_fields: JsonText",
                      s.template_schema AS "template_schema: JsonText",
                      s.template_submitters AS "template_submitters: JsonText",
                      s.source, s.submitters_order, s.slug,
                      s.preferences AS "preferences: JsonText", s.account_id, s.expire_at,
                      s.name, s.variables_schema AS "variables_schema: JsonText",
                      s.variables AS "variables: JsonText", s.completed_at
               FROM submissions s WHERE s.id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?)
}

/// Filters of `GET /api/submissions`, `Submissions::Filter` and `Submissions.plain_search`.
#[derive(Clone, Debug, Default)]
pub struct SubmissionFilter<'a> {
    pub account_id: i64,
    pub template_id: Option<i64>,
    pub slug: Option<&'a str>,
    /// Lowercase escaped `LIKE` pattern matched against submitter email, phone and name.
    pub search_like: Option<&'a str>,
    /// `Submissions.fulltext_search`: only these ids.
    pub ids: Option<&'a [i64]>,
    pub folder_ids: Option<&'a [i64]>,
    /// `None`: both; `Some(true)`: archived only; `Some(false)`: active only.
    pub archived: Option<bool>,
    /// `-1` matches no user, as `created_by_user_id: user&.id || -1` does.
    pub created_by_user_id: Option<i64>,
    /// `pending`, `completed`, `declined`, `expired`, `sent`, `opened`,
    /// `partially_completed`; anything else is ignored.
    pub status: Option<&'a str>,
    pub created_at_from: Option<NaiveDateTime>,
    pub created_at_to: Option<NaiveDateTime>,
    pub completed_at_from: Option<NaiveDateTime>,
    pub completed_at_to: Option<NaiveDateTime>,
}

pub async fn list(
    db: &PgPool,
    filter: &SubmissionFilter<'_>,
    page: Page,
) -> Result<Vec<Submission>, DbError> {
    let completed_filter = filter.completed_at_from.is_some() || filter.completed_at_to.is_some();
    Ok(sqlx::query_as!(
        Submission,
        r#"SELECT s.id, s.template_id, s.created_by_user_id, s.archived_at, s.created_at,
                      s.updated_at, s.template_fields AS "template_fields: JsonText",
                      s.template_schema AS "template_schema: JsonText",
                      s.template_submitters AS "template_submitters: JsonText",
                      s.source, s.submitters_order, s.slug,
                      s.preferences AS "preferences: JsonText", s.account_id, s.expire_at,
                      s.name, s.variables_schema AS "variables_schema: JsonText",
                      s.variables AS "variables: JsonText", s.completed_at
               FROM submissions s WHERE s.account_id = $1
                 AND ($2::bigint IS NULL OR s.template_id = $2)
                 AND ($3::text IS NULL OR s.slug = $3)
                 AND ($4::text IS NULL OR EXISTS (
                       SELECT 1 FROM submitters r WHERE r.submission_id = s.id
                         AND (LOWER(r.email) LIKE $4 OR r.phone LIKE $4 OR LOWER(r.name) LIKE $4)))
                 AND ($5::bigint[] IS NULL OR EXISTS (
                       SELECT 1 FROM templates t WHERE t.id = s.template_id AND t.folder_id = ANY($5)))
                 AND ($6::boolean IS NULL
                      OR ($6 AND s.archived_at IS NOT NULL)
                      OR (NOT $6 AND s.archived_at IS NULL))
                 AND ($7::bigint IS NULL OR s.created_by_user_id = $7)
                 AND (CASE $8::text
                   WHEN 'pending' THEN s.completed_at IS NULL
                     AND (s.expire_at IS NULL OR s.expire_at >= (now() AT TIME ZONE 'utc'))
                   WHEN 'completed' THEN s.completed_at IS NOT NULL
                   WHEN 'declined' THEN EXISTS (SELECT 1 FROM submitters r
                     WHERE r.submission_id = s.id AND r.declined_at IS NOT NULL)
                   WHEN 'expired' THEN s.expire_at <= (now() AT TIME ZONE 'utc')
                     AND s.completed_at IS NULL
                   WHEN 'sent' THEN EXISTS (SELECT 1 FROM submitters r
                     WHERE r.submission_id = s.id AND r.opened_at IS NULL
                       AND r.completed_at IS NULL AND r.declined_at IS NULL
                       AND r.sent_at IS NOT NULL)
                   WHEN 'opened' THEN EXISTS (SELECT 1 FROM submitters r
                     WHERE r.submission_id = s.id AND r.completed_at IS NULL
                       AND r.declined_at IS NULL AND r.opened_at IS NOT NULL)
                   WHEN 'partially_completed' THEN s.completed_at IS NULL AND EXISTS (
                     SELECT 1 FROM submitters r
                     WHERE r.submission_id = s.id AND r.completed_at IS NOT NULL)
                   ELSE true END)
                 AND ($9::timestamp IS NULL OR s.id >= COALESCE((
                       SELECT x.id FROM submissions x WHERE x.created_at >= $9
                       ORDER BY x.created_at, x.id LIMIT 1), 9223372036854775807))
                 AND ($10::timestamp IS NULL OR s.id <= COALESCE((
                       SELECT x.id FROM submissions x WHERE x.created_at <= $10
                       ORDER BY x.created_at DESC, x.id DESC LIMIT 1), 0))
                 AND (NOT $11 OR s.completed_at IS NOT NULL)
                 AND ($12::timestamp IS NULL OR s.completed_at >= $12)
                 AND ($13::timestamp IS NULL OR s.completed_at <= $13)
                 AND ($14::bigint IS NULL OR s.id < $14)
                 AND ($15::bigint IS NULL OR s.id > $15)
                 AND ($17::bigint[] IS NULL OR s.id = ANY($17))
               ORDER BY s.id DESC
               LIMIT $16"#,
        filter.account_id,
        filter.template_id,
        filter.slug,
        filter.search_like,
        filter.folder_ids,
        filter.archived,
        filter.created_by_user_id,
        filter.status,
        filter.created_at_from,
        filter.created_at_to,
        completed_filter,
        filter.completed_at_from,
        filter.completed_at_to,
        page.after,
        page.before,
        page.limit,
        filter.ids,
    )
    .fetch_all(db)
    .await?)
}

/// Submitters of the submissions, by id.
pub async fn submitters_for(
    db: impl PgExecutor<'_>,
    submission_ids: &[i64],
) -> Result<Vec<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                      r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                      r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                      r.preferences AS "preferences: JsonText",
                      r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                      r.timezone, r.template_id
               FROM submitters r WHERE r.submission_id = ANY($1) ORDER BY r.id"#,
        submission_ids
    )
    .fetch_all(db)
    .await?)
}

pub async fn find_submitter(
    db: impl PgExecutor<'_>,
    id: i64,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                      r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                      r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                      r.preferences AS "preferences: JsonText",
                      r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                      r.timezone, r.template_id
               FROM submitters r WHERE r.id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?)
}

pub async fn find_submitter_by_slug(
    db: impl PgExecutor<'_>,
    slug: &str,
) -> Result<Option<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                      r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                      r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                      r.preferences AS "preferences: JsonText",
                      r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                      r.timezone, r.template_id
               FROM submitters r WHERE r.slug = $1"#,
        slug
    )
    .fetch_optional(db)
    .await?)
}

/// Filters of `GET /api/submitters`.
#[derive(Clone, Debug, Default)]
pub struct SubmitterFilter<'a> {
    pub account_id: i64,
    /// Lowercase escaped `LIKE` pattern matched against email, phone and name.
    pub search_like: Option<&'a str>,
    /// `Submitters.fulltext_search`: only these ids.
    pub ids: Option<&'a [i64]>,
    pub external_id: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub submission_id: Option<i64>,
    pub template_id: Option<i64>,
    pub completed_after: Option<NaiveDateTime>,
    pub completed_before: Option<NaiveDateTime>,
}

pub async fn list_submitters(
    db: &PgPool,
    filter: &SubmitterFilter<'_>,
    page: Page,
) -> Result<Vec<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                      r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                      r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                      r.preferences AS "preferences: JsonText",
                      r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                      r.timezone, r.template_id
               FROM submitters r WHERE r.account_id = $1
                 AND ($2::text IS NULL
                      OR LOWER(r.email) LIKE $2 OR r.phone LIKE $2 OR LOWER(r.name) LIKE $2)
                 AND ($3::text IS NULL OR r.external_id = $3)
                 AND ($4::text IS NULL OR r.slug = $4)
                 AND ($5::bigint IS NULL OR r.submission_id = $5)
                 AND ($6::bigint IS NULL OR r.template_id = $6)
                 AND ($7::timestamp IS NULL OR r.completed_at >= $7)
                 AND ($8::timestamp IS NULL OR r.completed_at <= $8)
                 AND ($9::bigint IS NULL OR r.id < $9)
                 AND ($10::bigint IS NULL OR r.id > $10)
                 AND ($12::bigint[] IS NULL OR r.id = ANY($12))
               ORDER BY r.id DESC
               LIMIT $11"#,
        filter.account_id,
        filter.search_like,
        filter.external_id,
        filter.slug,
        filter.submission_id,
        filter.template_id,
        filter.completed_after,
        filter.completed_before,
        page.after,
        page.before,
        page.limit,
        filter.ids,
    )
    .fetch_all(db)
    .await?)
}

/// Events of the submissions, in id order.
pub async fn events_for_submissions(
    db: impl PgExecutor<'_>,
    submission_ids: &[i64],
) -> Result<Vec<SubmissionEvent>, DbError> {
    Ok(sqlx::query_as!(
        SubmissionEvent,
        r#"SELECT id, submission_id, submitter_id, data AS "data: JsonText", event_type,
                  event_timestamp, created_at, updated_at, account_id
           FROM submission_events WHERE submission_id = ANY($1) ORDER BY id"#,
        submission_ids
    )
    .fetch_all(db)
    .await?)
}

pub async fn insert_event(
    db: impl PgExecutor<'_>,
    submitter: &Submitter,
    event_type: &str,
    data: &JsonText,
) -> Result<SubmissionEvent, DbError> {
    Ok(sqlx::query_as!(
        SubmissionEvent,
        r#"INSERT INTO submission_events
             (submission_id, submitter_id, data, event_type, event_timestamp, account_id,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, (now() AT TIME ZONE 'utc'), $5,
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id, submission_id, submitter_id, data AS "data: JsonText", event_type,
                     event_timestamp, created_at, updated_at, account_id"#,
        submitter.submission_id,
        submitter.id,
        data as _,
        event_type,
        submitter.account_id,
    )
    .fetch_one(db)
    .await?)
}

/// `update!(archived_at: …)` for the submission. Returns the stored value.
pub async fn set_archived(
    db: impl PgExecutor<'_>,
    id: i64,
    archived: bool,
) -> Result<Option<NaiveDateTime>, DbError> {
    Ok(sqlx::query_scalar!(
        r#"UPDATE submissions SET
             archived_at = CASE WHEN $2 THEN (now() AT TIME ZONE 'utc') ELSE NULL END,
             updated_at = (now() AT TIME ZONE 'utc')
           WHERE id = $1 RETURNING archived_at"#,
        id,
        archived
    )
    .fetch_one(db)
    .await?)
}

/// The editable columns of `PUT /api/submissions/:id`.
#[derive(Clone, Debug)]
pub struct SubmissionUpdate {
    pub name: Option<String>,
    pub expire_at: Option<NaiveDateTime>,
    pub archived_at: Option<NaiveDateTime>,
}

/// Saves the changes, bumping `updated_at` only when something changed.
pub async fn update(
    db: impl PgExecutor<'_>,
    id: i64,
    changes: &SubmissionUpdate,
) -> Result<Submission, DbError> {
    Ok(sqlx::query_as!(
        Submission,
        r#"UPDATE submissions SET name = $2, expire_at = $3, archived_at = $4,
             updated_at = CASE
               WHEN (name, expire_at, archived_at) IS NOT DISTINCT FROM ($2::text, $3::timestamp, $4::timestamp)
               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1
           RETURNING id, template_id, created_by_user_id, archived_at, created_at, updated_at,
                     template_fields AS "template_fields: JsonText",
                     template_schema AS "template_schema: JsonText",
                     template_submitters AS "template_submitters: JsonText",
                     source, submitters_order, slug, preferences AS "preferences: JsonText",
                     account_id, expire_at, name,
                     variables_schema AS "variables_schema: JsonText",
                     variables AS "variables: JsonText", completed_at"#,
        id,
        changes.name,
        changes.expire_at,
        changes.archived_at,
    )
    .fetch_one(db)
    .await?)
}

/// `submitter.opened_at = Time.current; submitter.save`.
pub async fn mark_opened(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE submitters SET opened_at = (now() AT TIME ZONE 'utc'),
                               updated_at = (now() AT TIME ZONE 'utc')
         WHERE id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// A `completed_at` cursor page (`paginate(field: :completed_at)`), newest first.
#[derive(Clone, Copy, Debug)]
pub struct TimePage {
    pub limit: i64,
    /// Only completions before this time.
    pub after: Option<NaiveDateTime>,
    /// Only completions at or after this time (`before + 1` second).
    pub before: Option<NaiveDateTime>,
}

/// Completed submitters of an account (`/api/events/form/completed`).
pub async fn completed_submitters(
    db: impl PgExecutor<'_>,
    account_id: i64,
    page: TimePage,
) -> Result<Vec<Submitter>, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"SELECT r.id, r.submission_id, r.uuid, r.email, r.slug,
                      r.values AS "values: JsonText", r.ua, r.ip, r.sent_at, r.opened_at,
                      r.completed_at, r.created_at, r.updated_at, r.name, r.phone, r.external_id,
                      r.preferences AS "preferences: JsonText",
                      r.metadata AS "metadata: JsonText", r.account_id, r.declined_at,
                      r.timezone, r.template_id
               FROM submitters r WHERE r.account_id = $1 AND r.completed_at IS NOT NULL
             AND ($2::timestamp IS NULL OR r.completed_at < $2)
             AND ($3::timestamp IS NULL OR r.completed_at >= $3)
           ORDER BY r.completed_at DESC LIMIT $4"#,
        account_id,
        page.after,
        page.before,
        page.limit,
    )
    .fetch_all(db)
    .await?)
}

/// Completed, active submissions of an account (`/api/events/submission/completed`).
pub async fn completed_submissions(
    db: impl PgExecutor<'_>,
    account_id: i64,
    page: TimePage,
) -> Result<Vec<Submission>, DbError> {
    Ok(sqlx::query_as!(
        Submission,
        r#"SELECT s.id, s.template_id, s.created_by_user_id, s.archived_at, s.created_at,
                      s.updated_at, s.template_fields AS "template_fields: JsonText",
                      s.template_schema AS "template_schema: JsonText",
                      s.template_submitters AS "template_submitters: JsonText",
                      s.source, s.submitters_order, s.slug,
                      s.preferences AS "preferences: JsonText", s.account_id, s.expire_at,
                      s.name, s.variables_schema AS "variables_schema: JsonText",
                      s.variables AS "variables: JsonText", s.completed_at
               FROM submissions s WHERE s.account_id = $1 AND s.archived_at IS NULL
             AND s.completed_at IS NOT NULL
             AND ($2::timestamp IS NULL OR s.completed_at < $2)
             AND ($3::timestamp IS NULL OR s.completed_at >= $3)
           ORDER BY s.completed_at DESC LIMIT $4"#,
        account_id,
        page.after,
        page.before,
        page.limit,
    )
    .fetch_all(db)
    .await?)
}

/// A submission to insert (`template.submissions.new(...)`).
#[derive(Clone, Debug)]
pub struct NewSubmission {
    pub template_id: Option<i64>,
    pub created_by_user_id: Option<i64>,
    pub account_id: i64,
    pub slug: String,
    pub source: String,
    pub submitters_order: String,
    pub name: Option<String>,
    pub preferences: JsonText,
    pub variables: Option<JsonText>,
    pub expire_at: Option<NaiveDateTime>,
    pub template_fields: Option<JsonText>,
    pub template_schema: Option<JsonText>,
    pub template_submitters: Option<JsonText>,
    pub variables_schema: Option<JsonText>,
}

pub async fn insert(db: impl PgExecutor<'_>, new: &NewSubmission) -> Result<Submission, DbError> {
    Ok(sqlx::query_as!(
        Submission,
        r#"INSERT INTO submissions
             (template_id, created_by_user_id, account_id, slug, source, submitters_order, name,
              preferences, variables, expire_at, template_fields, template_schema,
              template_submitters, variables_schema, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id, template_id, created_by_user_id, archived_at, created_at, updated_at,
                     template_fields AS "template_fields: JsonText",
                     template_schema AS "template_schema: JsonText",
                     template_submitters AS "template_submitters: JsonText",
                     source, submitters_order, slug, preferences AS "preferences: JsonText",
                     account_id, expire_at, name,
                     variables_schema AS "variables_schema: JsonText",
                     variables AS "variables: JsonText", completed_at"#,
        new.template_id,
        new.created_by_user_id,
        new.account_id,
        new.slug,
        new.source,
        new.submitters_order,
        new.name,
        new.preferences as _,
        new.variables as _,
        new.expire_at,
        new.template_fields as _,
        new.template_schema as _,
        new.template_submitters as _,
        new.variables_schema as _,
    )
    .fetch_one(db)
    .await?)
}

/// A submitter to insert (`submission.submitters.new(...)`).
#[derive(Clone, Debug)]
pub struct NewSubmitter {
    pub submission_id: i64,
    pub template_id: Option<i64>,
    pub account_id: i64,
    pub uuid: String,
    pub slug: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub name: Option<String>,
    pub external_id: Option<String>,
    pub values: JsonText,
    pub metadata: JsonText,
    pub preferences: JsonText,
    pub sent_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
}

pub async fn insert_submitter(
    db: impl PgExecutor<'_>,
    new: &NewSubmitter,
) -> Result<Submitter, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"INSERT INTO submitters
             (submission_id, template_id, account_id, uuid, slug, email, phone, name,
              external_id, values, metadata, preferences, sent_at, completed_at, created_at,
              updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                   (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING id, submission_id, uuid, email, slug, values AS "values: JsonText", ua, ip,
                     sent_at, opened_at, completed_at, created_at, updated_at, name, phone,
                     external_id, preferences AS "preferences: JsonText",
                     metadata AS "metadata: JsonText", account_id, declined_at, timezone,
                     template_id"#,
        new.submission_id,
        new.template_id,
        new.account_id,
        new.uuid,
        new.slug,
        new.email,
        new.phone,
        new.name,
        new.external_id,
        new.values as _,
        new.metadata as _,
        new.preferences as _,
        new.sent_at,
        new.completed_at,
    )
    .fetch_one(db)
    .await?)
}

/// The editable columns of `PUT /api/submitters/:id`.
#[derive(Clone, Debug)]
pub struct SubmitterUpdate {
    pub email: Option<String>,
    pub name: Option<String>,
    pub phone: Option<String>,
    pub external_id: Option<String>,
    pub values: JsonText,
    pub metadata: JsonText,
    pub preferences: JsonText,
    pub sent_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
}

/// Saves the changes and bumps `updated_at` when anything changed.
pub async fn update_submitter(
    db: impl PgExecutor<'_>,
    id: i64,
    changes: &SubmitterUpdate,
) -> Result<Submitter, DbError> {
    Ok(sqlx::query_as!(
        Submitter,
        r#"UPDATE submitters SET
             email = $2, name = $3, phone = $4, external_id = $5, values = $6, metadata = $7,
             preferences = $8, sent_at = $9, completed_at = $10,
             updated_at = CASE
               WHEN (email, name, phone, external_id, values, metadata, preferences, sent_at,
                     completed_at)
                    IS NOT DISTINCT FROM
                    ($2::varchar, $3::varchar, $4::varchar, $5::varchar, $6::text, $7::text,
                     $8::text, $9::timestamp, $10::timestamp)
               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1
           RETURNING id, submission_id, uuid, email, slug, values AS "values: JsonText", ua, ip,
                     sent_at, opened_at, completed_at, created_at, updated_at, name, phone,
                     external_id, preferences AS "preferences: JsonText",
                     metadata AS "metadata: JsonText", account_id, declined_at, timezone,
                     template_id"#,
        id,
        changes.email,
        changes.name,
        changes.phone,
        changes.external_id,
        changes.values as _,
        changes.metadata as _,
        changes.preferences as _,
        changes.sent_at,
        changes.completed_at,
    )
    .fetch_one(db)
    .await?)
}

/// Sets the submission's field and schema snapshots (`submission.save!` after
/// `maybe_set_template_fields`); bumps `updated_at` only on a change.
pub async fn update_snapshots(
    db: impl PgExecutor<'_>,
    id: i64,
    template_fields: Option<&JsonText>,
    template_schema: Option<&JsonText>,
    variables_schema: Option<&JsonText>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE submissions SET
             template_fields = $2, template_schema = $3, variables_schema = $4,
             updated_at = CASE
               WHEN (template_fields, template_schema, variables_schema)
                    IS NOT DISTINCT FROM ($2::text, $3::text, $4::text)
               THEN updated_at ELSE (now() AT TIME ZONE 'utc') END
           WHERE id = $1"#,
        id,
        template_fields as _,
        template_schema as _,
        variables_schema as _,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `EmailMessages.find_or_create_for_account_user` without asset extraction: the
/// message with this subject and body, by `sha1`. Returns its uuid.
pub async fn find_or_create_email_message(
    db: impl PgExecutor<'_> + Copy,
    account_id: i64,
    author_id: i64,
    subject: &str,
    body: &str,
    sha1: &str,
) -> Result<String, DbError> {
    if let Some(uuid) = sqlx::query_scalar!(
        "SELECT uuid FROM email_messages WHERE account_id = $1 AND sha1 = $2 ORDER BY id LIMIT 1",
        account_id,
        sha1
    )
    .fetch_optional(db)
    .await?
    {
        return Ok(uuid);
    }
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO email_messages
             (account_id, author_id, subject, body, sha1, uuid, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
           RETURNING uuid"#,
        account_id,
        author_id,
        subject,
        body,
        sha1,
        uuid::Uuid::new_v4().to_string(),
    )
    .fetch_one(db)
    .await?)
}

/// An account's email message by uuid: `(subject, body)`.
pub async fn find_email_message(
    db: impl PgExecutor<'_>,
    account_id: i64,
    uuid: &str,
) -> Result<Option<(String, String)>, DbError> {
    Ok(sqlx::query!(
        "SELECT subject, body FROM email_messages WHERE account_id = $1 AND uuid = $2
         ORDER BY id LIMIT 1",
        account_id,
        uuid
    )
    .fetch_optional(db)
    .await?
    .map(|r| (r.subject, r.body)))
}

/// `EmailMessageAsset` data by sha1 for `rebuild_body_with_assets`.
pub async fn email_message_assets(
    db: impl PgExecutor<'_>,
    account_id: i64,
    sha1s: &[String],
) -> Result<Vec<(String, String)>, DbError> {
    Ok(sqlx::query!(
        "SELECT sha1, data FROM email_message_assets WHERE account_id = $1 AND sha1 = ANY($2)",
        account_id,
        sha1s
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| (r.sha1, r.data))
    .collect())
}

/// An `EmailEvent` (`ActionMailerEventsObserver#delivered_email`).
pub struct NewEmailEvent<'a> {
    pub account_id: i64,
    pub tag: &'a str,
    pub message_id: &'a str,
    pub emailable_type: &'a str,
    pub emailable_id: i64,
    pub event_type: &'a str,
    pub email: &'a str,
    pub data: &'a JsonText,
}

pub async fn insert_email_event(
    db: impl PgExecutor<'_>,
    e: &NewEmailEvent<'_>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"INSERT INTO email_events
             (account_id, tag, message_id, emailable_type, emailable_id, event_type, email, data,
              event_datetime, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, (now() AT TIME ZONE 'utc'),
                   (now() AT TIME ZONE 'utc'))"#,
        e.account_id,
        e.tag,
        e.message_id,
        e.emailable_type,
        e.emailable_id,
        e.event_type,
        e.email,
        e.data as _,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `submitter.sent_at ||= Time.current; submitter.save!`.
pub async fn mark_sent(db: impl PgExecutor<'_>, id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE submitters SET sent_at = (now() AT TIME ZONE 'utc'),
                               updated_at = (now() AT TIME ZONE 'utc')
         WHERE id = $1 AND sent_at IS NULL",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `AccountConfigs.find_for_account`: the account's config, else the first account's.
pub async fn account_config_with_fallback(
    db: impl PgExecutor<'_>,
    account_id: i64,
    key: &str,
) -> Result<Option<serde_json::Value>, DbError> {
    let value: Option<String> = sqlx::query_scalar!(
        r#"SELECT c.value AS "value!" FROM account_configs c
           WHERE c.key = $2 AND c.account_id IN ($1, (SELECT id FROM accounts ORDER BY id LIMIT 1))
           ORDER BY (c.account_id = $1) DESC, c.id LIMIT 1"#,
        account_id,
        key
    )
    .fetch_optional(db)
    .await?;
    Ok(value.map(|v| serde_json::from_str(&v).unwrap_or(serde_json::Value::Null)))
}
