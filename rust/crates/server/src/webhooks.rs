//! Webhooks: `WebhookUrls.enqueue_events`, the `Send*WebhookRequestJob`s and
//! `SendWebhookRequest` (signed with `X-Docuseal-Signature`, retried after `2^attempt`
//! minutes until the job's maximum attempt).

use chrono::{Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use loco_db::{
    submissions, templates,
    webhooks::{self, WebhookUrl},
};
use loco_jobs::{EnqueueOptions, Job};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{
    api::serialize::{self, Load, SubmissionData, SubmissionOpts, TemplateData, Urls},
    errors::AppError,
    state::AppState,
};

/// `SendWebhookRequest::USER_AGENT`.
pub const USER_AGENT: &str = "DocuSeal.com Webhook";
/// `SendWebhookRequest::MANUAL_ATTEMPT`: attempts at or above it resend even after success.
pub const MANUAL_ATTEMPT: i32 = 99_999;

/// The record a `form.*`, `submission.*` or `template.*` event is about.
#[derive(Clone, Copy, Debug)]
pub struct EventRecord {
    pub id: i64,
    pub account_id: i64,
}

/// `WebhookUrls::Signatures.sign`: `"#{timestamp}.#{hmac_sha256_hex}"`.
#[must_use]
pub fn sign(secret: &str, body: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key size");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("{timestamp}.{}", hex::encode(mac.finalize().into_bytes()))
}

/// `WebhookUrls.enqueue_events(records, event_type)`: one job per subscribed webhook
/// and record, sharing an event uuid per record.
pub async fn enqueue_events(
    state: &AppState,
    event_type: &str,
    records: &[EventRecord],
) -> anyhow::Result<()> {
    let mut accounts: Vec<i64> = records.iter().map(|r| r.account_id).collect();
    accounts.dedup();
    for account_id in accounts {
        let urls = webhooks::urls_for_account(&state.db, account_id, event_type).await?;
        if urls.is_empty() {
            continue;
        }
        for record in records.iter().filter(|r| r.account_id == account_id) {
            let event_uuid = uuid::Uuid::new_v4().to_string();
            for url in urls.iter().filter(|u| u.subscribes_to(event_type)) {
                loco_jobs::enqueue(
                    &state.db,
                    &SendWebhook {
                        event_type: event_type.to_owned(),
                        record_id: record.id,
                        webhook_url_id: url.id,
                        event_uuid: event_uuid.clone(),
                        attempt: 0,
                        last_status: None,
                    },
                    EnqueueOptions::default(),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Like [`enqueue_events`], logging instead of failing the request.
pub async fn enqueue(state: &AppState, event_type: &str, records: &[EventRecord]) {
    if let Err(err) = enqueue_events(state, event_type, records).await {
        tracing::error!(
            error = format!("{err:#}"),
            event_type,
            "enqueueing webhooks failed"
        );
    }
}

/// All `Send*WebhookRequestJob`s.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendWebhook {
    pub event_type: String,
    pub record_id: i64,
    pub webhook_url_id: i64,
    pub event_uuid: String,
    #[serde(default)]
    pub attempt: i32,
    #[serde(default)]
    pub last_status: Option<u16>,
}

impl SendWebhook {
    /// Each job's `MAX_ATTEMPTS`.
    fn max_attempts(&self) -> i32 {
        if self.event_type == "form.completed" {
            12
        } else {
            10
        }
    }

    fn record_type(&self) -> &'static str {
        match self.event_type.split('.').next() {
            Some("form") => "Submitter",
            Some("template") => "Template",
            _ => "Submission",
        }
    }
}

impl Job for SendWebhook {
    const KIND: &'static str = "send_webhook";
    const QUEUE: &'static str = "webhooks";
    const MAX_ATTEMPTS: i32 = 3;

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        let Some(webhook_url) = webhooks::find_url(&state.db, self.webhook_url_id).await? else {
            return Ok(());
        };
        if !webhook_url.subscribes_to(&self.event_type) {
            return Ok(());
        }
        let Some(data) = payload(&state, &self)
            .await
            .map_err(AppError::into_anyhow)?
        else {
            return Ok(());
        };
        let status = deliver(&state, &webhook_url, &self, data).await?;

        if self.attempt > self.max_attempts() || status.is_some_and(|s| s < 400) {
            return Ok(());
        }
        let retry = Self {
            attempt: self.attempt + 1,
            last_status: Some(status.unwrap_or(0)),
            ..self.clone()
        };
        loco_jobs::enqueue(
            &state.db,
            &retry,
            EnqueueOptions {
                run_at: Some(Utc::now() + Duration::minutes(1_i64 << self.attempt.clamp(0, 20))),
            },
        )
        .await?;
        Ok(())
    }
}

/// The event's `data`, or `None` when the record is gone.
async fn payload(state: &AppState, job: &SendWebhook) -> Result<Option<Value>, AppError> {
    // `*.archived` events carry `record.as_json(only: %i[id archived_at])`.
    let archived = job.event_type.ends_with(".archived");
    let archived_json = |id: i64, at: Option<chrono::NaiveDateTime>| json!({ "id": id, "archived_at": at.map(serialize::time) });
    match job.record_type() {
        "Template" => {
            let Some(template) = templates::find(&state.db, job.record_id).await? else {
                return Ok(None);
            };
            if archived {
                return Ok(Some(archived_json(template.id, template.archived_at)));
            }
            let urls = Urls::new(state, template.account_id).await?;
            let data = TemplateData::load(state, std::slice::from_ref(&template)).await?;
            Ok(Some(serialize::template_json(&urls, &data, &template)))
        }
        "Submitter" => {
            let Some(submitter) = submissions::find_submitter(&state.db, job.record_id).await?
            else {
                return Ok(None);
            };
            let Some(submission) = submissions::find(&state.db, submitter.submission_id).await?
            else {
                return Ok(None);
            };
            let urls = Urls::new(state, submitter.account_id).await?;
            let data = SubmissionData::load(
                state,
                std::slice::from_ref(&submission),
                Load {
                    events: true,
                    documents: true,
                    attachments: true,
                },
            )
            .await?;
            Ok(Some(serialize::webhook_submitter_json(
                &urls,
                &data,
                &submission,
                &submitter,
            )))
        }
        _ => {
            let Some(submission) = submissions::find(&state.db, job.record_id).await? else {
                return Ok(None);
            };
            if archived {
                return Ok(Some(archived_json(submission.id, submission.archived_at)));
            }
            let urls = Urls::new(state, submission.account_id).await?;
            let data = SubmissionData::load(
                state,
                std::slice::from_ref(&submission),
                Load {
                    events: true,
                    documents: true,
                    attachments: true,
                },
            )
            .await?;
            Ok(Some(serialize::submission_json(
                &urls,
                &data,
                &submission,
                SubmissionOpts::default(),
            )))
        }
    }
}

/// `SendWebhookRequest.call`: returns the response status, or `None` for a connection
/// error or a skipped delivery.
async fn deliver(
    state: &AppState,
    webhook_url: &WebhookUrl,
    job: &SendWebhook,
    data: Value,
) -> anyhow::Result<Option<u16>> {
    let encryptor = &state.secrets.encryptor;
    let url = encryptor.decrypt(&webhook_url.url)?;
    if url.trim().is_empty() {
        return Ok(Some(200));
    }
    let hmac_secret = encryptor.decrypt(&webhook_url.hmac_secret)?;
    let extra_headers: serde_json::Map<String, Value> = encryptor
        .decrypt(&webhook_url.secret)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let (event_id, status) = webhooks::find_or_create_event(
        &state.db,
        webhook_url,
        &job.event_uuid,
        &job.event_type,
        job.record_type(),
        job.record_id,
    )
    .await?;
    if (1..MANUAL_ATTEMPT).contains(&job.attempt) && status == "success" {
        return Ok(Some(200));
    }
    let created_at = webhooks::event_created_at(&state.db, event_id).await?;

    let body = json!({
        "event_type": job.event_type,
        "timestamp": serialize::time(created_at),
        "data": data,
    })
    .to_string();

    let mut request = state
        .http
        .post(&url)
        .timeout(std::time::Duration::from_secs(15))
        .header("content-type", "application/json")
        .header("user-agent", USER_AGENT);
    let mut signed = false;
    for (name, value) in &extra_headers {
        let Some(value) = serialize_header(value) else {
            continue;
        };
        signed |= name.eq_ignore_ascii_case("x-docuseal-signature") && !value.is_empty();
        request = request.header(name.as_str(), value);
    }
    if !signed {
        request = request.header(
            "x-docuseal-signature",
            sign(&hmac_secret, &body, Utc::now().timestamp()),
        );
    }

    match request.body(body).send().await {
        Ok(response) => {
            let code = response.status().as_u16();
            let is_error = code >= 400;
            let text = if is_error {
                Some(truncate(&response.text().await.unwrap_or_default(), 100))
            } else {
                None
            };
            webhooks::record_attempt(
                &state.db,
                event_id,
                job.attempt,
                i32::from(code),
                text.as_deref(),
                !is_error,
            )
            .await?;
            Ok(Some(code))
        }
        Err(err) => {
            let message = if err.is_timeout() {
                "TimeoutError".to_owned()
            } else if err.is_connect() {
                "ConnectionFailed".to_owned()
            } else {
                truncate(&err.to_string(), 100)
            };
            webhooks::record_attempt(&state.db, event_id, job.attempt, 0, Some(&message), false)
                .await?;
            Ok(None)
        }
    }
}

fn serialize_header(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// `String#truncate(n)`: at most `n` characters, ending in `...` when cut.
#[must_use]
pub fn truncate(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(n.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_like_webhook_urls_signatures() {
        // OpenSSL::HMAC.hexdigest('sha256', 'whsec_test', '1700000000.{"a":1}')
        assert_eq!(
            sign("whsec_test", "{\"a\":1}", 1_700_000_000),
            "1700000000.38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789"
        );
    }

    #[test]
    fn truncates_like_rails() {
        assert_eq!(truncate("short", 100), "short");
        assert_eq!(truncate(&"x".repeat(120), 100).len(), 100);
    }
}
