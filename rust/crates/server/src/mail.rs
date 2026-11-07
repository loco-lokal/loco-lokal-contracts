//! Submitter emails: `SendSubmitterInvitationEmailJob` and
//! `SubmitterMailer#invitation_email` / `#invitation_view_email`.

use std::sync::LazyLock;

use loco_db::{
    JsonText,
    submissions::{self, NewEmailEvent},
    users,
};
use loco_jobs::{EnqueueOptions, Job};
use loco_mailer::Email;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    email_variables::{self, Vars},
    markdown,
    page::PRODUCT_NAME,
    routes::passwords::email_origin,
    state::{AppState, EMAIL_SMTP_KEY},
};

const INVITATION_EMAIL_KEY: &str = "submitter_invitation_email";
const VIEW_INVITATION_EMAIL_KEY: &str = "submitter_view_invitation_email";
const INTEGRATION_FROM_EMAIL_KEY: &str = "integration_from_email";

static NO_REPLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)no-?reply@").expect("valid regex"));
static ASSET_PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[asset:([0-9a-fA-F]{40})\]\]").expect("valid regex"));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendInvitation {
    pub submitter_id: i64,
}

impl Job for SendInvitation {
    const KIND: &'static str = "send_submitter_invitation_email";
    const QUEUE: &'static str = "mailers";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        send_invitation(&state, self.submitter_id).await
    }
}

/// `SendSubmitterInvitationEmailJob.perform_async`. Failures are logged.
pub async fn invitation_later(state: &AppState, submitter_id: i64) {
    let job = SendInvitation { submitter_id };
    if let Err(err) = loco_jobs::enqueue(&state.db, &job, EnqueueOptions::default()).await {
        tracing::error!(%err, submitter_id, "enqueueing an invitation email failed");
    }
}

/// `Accounts.can_send_emails?` outside multitenant mode.
async fn can_send_emails(state: &AppState) -> anyhow::Result<bool> {
    if !state.config.production || std::env::var("SMTP_ADDRESS").is_ok_and(|v| !v.is_empty()) {
        return Ok(true);
    }
    Ok(loco_db::first_encrypted_config(&state.db, EMAIL_SMTP_KEY)
        .await?
        .is_some())
}

fn present(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// `EmailMessage#normalized_body`: `[[asset:sha1]]` placeholders are replaced by the
/// stored asset data.
async fn normalized_body(
    state: &AppState,
    account_id: i64,
    body: String,
) -> anyhow::Result<String> {
    if !body.contains("[[asset:") {
        return Ok(body);
    }
    let mut sha1s: Vec<String> = ASSET_PLACEHOLDER
        .captures_iter(&body)
        .map(|c| c[1].to_owned())
        .collect();
    sha1s.dedup();
    let assets: std::collections::HashMap<String, String> =
        submissions::email_message_assets(&state.db, account_id, &sha1s)
            .await?
            .into_iter()
            .collect();
    Ok(ASSET_PLACEHOLDER
        .replace_all(&body, |c: &regex::Captures<'_>| {
            assets
                .get(&c[1])
                .cloned()
                .unwrap_or_else(|| c[0].to_owned())
        })
        .into_owned())
}

/// `TextUtils.rtl?`: Hebrew or Arabic script.
fn rtl(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x0590..=0x05FF | 0xFB1D..=0xFB4F
            | 0x0600..=0x06FF | 0x0750..=0x077F | 0x0870..=0x08FF
            | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
    })
}

/// The `dir` of the mailer layout's body follows the content, without the attribution.
fn apply_direction(html: String) -> String {
    const BODY: &str = "<body dir=\"auto\">";
    let Some(start) = html.find(BODY) else {
        return html;
    };
    let content_start = start + BODY.len();
    let content_end = html
        .rfind("<p>\n  ---")
        .filter(|end| *end >= content_start)
        .unwrap_or(html.len());
    if rtl(&html[content_start..content_end]) {
        html.replacen(BODY, "<body dir=\"rtl\">", 1)
    } else {
        html
    }
}

async fn send_invitation(state: &AppState, submitter_id: i64) -> anyhow::Result<()> {
    let Some(submitter) = submissions::find_submitter(&state.db, submitter_id).await? else {
        anyhow::bail!("submitter {submitter_id} not found");
    };
    let Some(mut vars) = Vars::load(state, submitter)
        .await
        .map_err(crate::errors::AppError::into_anyhow)?
    else {
        anyhow::bail!("submission of submitter {submitter_id} not found");
    };
    let now = chrono::Utc::now().naive_utc();
    if vars.submitter.completed_at.is_some()
        || vars.submitter.declined_at.is_some()
        || vars.submission.archived_at.is_some()
        || vars.submission.expired(now)
        || vars
            .template
            .as_ref()
            .is_some_and(|t| t.archived_at.is_some())
    {
        return Ok(());
    }
    if vars.submission.source == "invite" && !can_send_emails(state).await? {
        return Ok(());
    }

    let account_id = vars.submission.account_id;
    let viewer = vars.viewer();

    // Subject and body: the request's message, the template's per-recipient or global
    // preferences, then the account's email settings.
    let email_message = match present(vars.submitter.preferences.get("email_message_uuid")) {
        Some(uuid) => {
            submissions::find_email_message(&state.db, vars.submitter.account_id, &uuid).await?
        }
        None => None,
    };
    let template_preferences = vars
        .template
        .as_ref()
        .map_or(Value::Null, |t| t.preferences.0.clone());
    let recipient_preferences = if email_message.is_some() {
        None
    } else {
        template_preferences
            .get("submitters")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter().rev().find(|e| {
                    e.get("uuid").and_then(Value::as_str) == Some(vars.submitter.uuid.as_str())
                })
            })
            .cloned()
    };
    let recipient = |key: &str| present(recipient_preferences.as_ref().and_then(|p| p.get(key)));
    let template_pref = |key: &str| present(template_preferences.get(key));

    let (message_subject, message_body) = match email_message {
        Some((subject, body)) => (
            Some(subject).filter(|s| !s.trim().is_empty()),
            Some(normalized_body(state, account_id, body).await?).filter(|b| !b.trim().is_empty()),
        ),
        None => (None, None),
    };
    let (mut body, subject) = if viewer {
        (
            message_body
                .or_else(|| template_pref("invitation_view_email_body"))
                .or_else(|| recipient("request_email_body")),
            message_subject
                .or_else(|| template_pref("invitation_view_email_subject"))
                .or_else(|| recipient("request_email_subject")),
        )
    } else {
        (
            message_body
                .or_else(|| recipient("request_email_body"))
                .or_else(|| template_pref("request_email_body")),
            message_subject
                .or_else(|| recipient("request_email_subject"))
                .or_else(|| template_pref("request_email_subject")),
        )
    };
    let config_key = if viewer {
        VIEW_INVITATION_EMAIL_KEY
    } else {
        INVITATION_EMAIL_KEY
    };
    let email_config =
        submissions::account_config_with_fallback(&state.db, account_id, config_key).await?;
    if body.is_none() {
        body = email_config.as_ref().and_then(|c| present(c.get("body")));
    }

    let reply_to = build_reply_to(state, &vars, email_config.as_ref()).await?;
    let from = from_address(state, &vars).await?;
    let locale = vars.account.locale.clone();

    // `build_invite_subject`.
    let subject = match (&subject, &email_config) {
        (Some(_), _) | (None, Some(_)) => {
            let text = subject
                .clone()
                .or_else(|| {
                    email_config
                        .as_ref()
                        .and_then(|c| c.get("subject"))
                        .map(crate::api::create::values::ruby_to_s)
                })
                .unwrap_or_default();
            vars.replace(state, &text)
                .await
                .map_err(crate::errors::AppError::into_anyhow)?
        }
        (None, None) => state.views.t(
            &locale,
            if viewer {
                "you_are_invited_to_view_a_document"
            } else if vars.with_signature_fields() {
                "you_are_invited_to_sign_a_document"
            } else {
                "you_are_invited_to_submit_a_form"
            },
            &[],
        ),
    };

    let link = format!(
        "{}/s/{}?t={}",
        email_origin(state),
        vars.submitter.slug,
        email_variables::tracking_param(
            &state.config.secret_key_base,
            &vars.submitter.slug,
            "click_email"
        )
    );
    let custom_body = match &body {
        Some(body) => Some(markdown::to_html(
            &vars
                .replace(state, body)
                .await
                .map_err(crate::errors::AppError::into_anyhow)?,
        )),
        None => None,
    };
    let show_link = body
        .as_deref()
        .is_some_and(|b| !email_variables::links_to_form(b))
        && !matches!(vars.submission.source.as_str(), "api" | "embed");
    let template = if viewer {
        "submitter_mailer/invitation_view_email.html"
    } else {
        "submitter_mailer/invitation_email.html"
    };
    let html = apply_direction(state.views.render(
        template,
        json!({
            "locale": locale,
            "product_name": PRODUCT_NAME,
            "custom_body": custom_body,
            "show_link": show_link,
            "link": link,
            "with_signature_fields": vars.with_signature_fields(),
            "name": vars.submission_name(),
            "account_name": vars.account.name,
        }),
    )?);

    // `Submitters::ValidateSending`.
    let to = vars.submitter.friendly_name().unwrap_or_default();
    if !vars
        .submitter
        .email
        .as_deref()
        .unwrap_or_default()
        .contains('@')
    {
        anyhow::bail!("Submitters::ValidateSending::InvalidEmail");
    }

    let message_uuid = uuid::Uuid::new_v4().to_string();
    let email = Email {
        from,
        to: vec![to],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to,
        subject,
        html,
        text: None,
        attachments: Vec::new(),
        headers: vec![("X-Message-Uuid".to_owned(), message_uuid.clone())],
    };
    let delivered = state.mailer.deliver(&email).await?;

    // `ActionMailerEventsObserver#delivered_email`.
    let tag = if viewer {
        "submitter_view_invitation"
    } else {
        "submitter_invitation"
    };
    let data = JsonText(json!({ "from": delivered.from, "method": delivered.method }));
    for address in &delivered.to {
        let event = NewEmailEvent {
            account_id,
            tag,
            message_id: &message_uuid,
            emailable_type: "Submitter",
            emailable_id: vars.submitter.id,
            event_type: "send",
            email: address,
            data: &data,
        };
        if let Err(err) = submissions::insert_email_event(&state.db, &event).await {
            tracing::error!(%err, "recording an email event failed");
        }
    }

    submissions::insert_event(
        &state.db,
        &vars.submitter,
        "send_email",
        &JsonText(json!({})),
    )
    .await?;
    submissions::mark_sent(&state.db, vars.submitter.id).await?;
    Ok(())
}

/// `build_submitter_reply_to`.
async fn build_reply_to(
    state: &AppState,
    vars: &Vars,
    email_config: Option<&Value>,
) -> anyhow::Result<Option<String>> {
    let mut reply_to = present(vars.submitter.preferences.get("reply_to"))
        .or_else(|| email_config.and_then(|c| present(c.get("reply_to"))));
    if reply_to.is_none() {
        let sender = sender_user(state, vars).await?;
        if sender.as_ref().map(|u| Some(u.email.as_str())) != Some(vars.submitter.email.as_deref())
        {
            reply_to = sender.map(|u| email_variables::plus_address_removed(&u.friendly_name()));
        }
    }
    Ok(reply_to.filter(|r| !NO_REPLY.is_match(r)))
}

/// `submission.created_by_user || template.author`.
async fn sender_user(state: &AppState, vars: &Vars) -> anyhow::Result<Option<users::User>> {
    if let Some(user) = &vars.created_by {
        return Ok(Some(user.clone()));
    }
    match &vars.template {
        Some(template) => Ok(users::find(&state.db, template.author_id).await?),
        None => Ok(None),
    }
}

/// `from_address_for_submitter`: the integration sender for API and embedded requests,
/// else the creator.
async fn from_address(state: &AppState, vars: &Vars) -> anyhow::Result<Option<String>> {
    if matches!(vars.submission.source.as_str(), "api" | "embed")
        && let Some(from) = present(
            users::account_config(
                &state.db,
                vars.submitter.account_id,
                INTEGRATION_FROM_EMAIL_KEY,
            )
            .await?
            .as_ref(),
        )
    {
        return Ok(Some(from));
    }
    Ok(sender_user(state, vars).await?.map(|u| u.friendly_name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rtl_content() {
        assert!(rtl("שלום"));
        assert!(rtl("مرحبا"));
        assert!(!rtl("Hello"));
    }

    #[test]
    fn sets_direction_from_content_only() {
        let html = "<body dir=\"auto\"><p>שלום</p><p>\n  ---\n</p></body>".to_owned();
        assert!(apply_direction(html).contains("dir=\"rtl\""));
        let html = "<body dir=\"auto\"><p>Hi</p><p>\n  ---\n</p><p>שלום</p></body>".to_owned();
        assert!(apply_direction(html).contains("dir=\"auto\""));
    }
}
