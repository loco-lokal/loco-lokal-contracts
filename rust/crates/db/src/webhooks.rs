//! `webhook_urls`, `webhook_events` and `webhook_attempts`.

use sqlx::{PgExecutor, PgPool};

use crate::{DbError, JsonText};

/// A `webhook_urls` row. `url`, `secret` and `hmac_secret` are Active Record–encrypted;
/// `secret` decrypts to a JSON object of extra request headers.
#[derive(Clone, Debug)]
pub struct WebhookUrl {
    pub id: i64,
    pub account_id: i64,
    pub events: JsonText,
    pub url: String,
    pub secret: String,
    pub hmac_secret: String,
}

impl WebhookUrl {
    #[must_use]
    pub fn subscribes_to(&self, event_type: &str) -> bool {
        self.events
            .as_array()
            .is_some_and(|events| events.iter().any(|e| e.as_str() == Some(event_type)))
    }
}

pub async fn find_url(db: impl PgExecutor<'_>, id: i64) -> Result<Option<WebhookUrl>, DbError> {
    Ok(sqlx::query_as!(
        WebhookUrl,
        r#"SELECT id, account_id, events AS "events: JsonText", url, secret, hmac_secret
           FROM webhook_urls WHERE id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?)
}

/// `WebhookUrls.for_account_id(account_id, event_type)` (single-tenant): the account's
/// webhooks subscribed to the event; when the account has none at all, those of the
/// accounts it is linked to (other than as a testing account).
pub async fn urls_for_account(
    db: &PgPool,
    account_id: i64,
    event_type: &str,
) -> Result<Vec<WebhookUrl>, DbError> {
    let pattern = format!("%\"{event_type}\"%");
    let rows = sqlx::query_as!(
        WebhookUrl,
        r#"SELECT id AS "id!", account_id AS "account_id!", events AS "events!: JsonText",
                  url AS "url!", secret AS "secret!", hmac_secret AS "hmac_secret!"
           FROM webhook_urls w
           WHERE w.account_id = $1
              OR ($1 <> 1 AND w.events LIKE $2 AND w.account_id IN (
                    SELECT l.account_id FROM account_linked_accounts l
                    WHERE l.linked_account_id = $1 AND l.account_type <> 'testing'))
           ORDER BY w.id"#,
        account_id,
        pattern,
    )
    .fetch_all(db)
    .await?;

    let (own, linked): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|w| w.account_id == account_id);
    let subscribed: Vec<_> = own
        .iter()
        .filter(|w| w.subscribes_to(event_type))
        .cloned()
        .collect();
    Ok(if !subscribed.is_empty() {
        subscribed
    } else if own.is_empty() && account_id != 1 {
        linked
    } else {
        Vec::new()
    })
}

/// `WebhookEvent.create_with(...).find_or_create_by!(webhook_url:, uuid:)`: returns the
/// event id and status.
pub async fn find_or_create_event(
    db: &PgPool,
    webhook_url: &WebhookUrl,
    event_uuid: &str,
    event_type: &str,
    record_type: &str,
    record_id: i64,
) -> Result<(i64, String), DbError> {
    let row = sqlx::query!(
        r#"WITH inserted AS (
             INSERT INTO webhook_events
               (uuid, webhook_url_id, account_id, event_type, record_type, record_id, status,
                created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'pending',
                     (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))
             ON CONFLICT (uuid, webhook_url_id) DO NOTHING
             RETURNING id, status, created_at
           )
           SELECT id AS "id!", status AS "status!", created_at AS "created_at!" FROM inserted
           UNION ALL
           SELECT id, status, created_at FROM webhook_events
           WHERE uuid = $1 AND webhook_url_id = $2
           LIMIT 1"#,
        event_uuid,
        webhook_url.id,
        webhook_url.account_id,
        event_type,
        record_type,
        record_id,
    )
    .fetch_one(db)
    .await?;
    Ok((row.id, row.status))
}

/// When the event was created (the payload's `timestamp`).
pub async fn event_created_at(
    db: impl PgExecutor<'_>,
    event_id: i64,
) -> Result<chrono::NaiveDateTime, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT created_at FROM webhook_events WHERE id = $1",
        event_id
    )
    .fetch_one(db)
    .await?)
}

/// Records an attempt and sets the event's status.
pub async fn record_attempt(
    db: &PgPool,
    event_id: i64,
    attempt: i32,
    status_code: i32,
    response_body: Option<&str>,
    success: bool,
) -> Result<(), DbError> {
    let mut tx = db.begin().await?;
    sqlx::query!(
        r#"INSERT INTO webhook_attempts
             (webhook_event_id, attempt, response_status_code, response_body, created_at,
              updated_at)
           VALUES ($1, $2, $3, $4, (now() AT TIME ZONE 'utc'), (now() AT TIME ZONE 'utc'))"#,
        event_id,
        attempt,
        status_code,
        response_body,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE webhook_events SET status = $2, updated_at = (now() AT TIME ZONE 'utc')
         WHERE id = $1",
        event_id,
        if success { "success" } else { "error" },
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
