//! The emails of a completed submission (`ProcessSubmitterCompletionJob#enqueue_completed_emails`):
//! `SubmitterMailer#completed_email` to the sender and the BCC addresses, and
//! `#documents_copy_email` to the parties, with the result documents and the audit
//! trail attached.

use std::sync::LazyLock;

use loco_db::{signing, submissions::Submitter, users};
use loco_jobs::{EnqueueOptions, Job};
use loco_mailer::{Email, EmailAttachment};
use loco_storage::{Attachment, Blob};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Form;
use crate::{
    email_variables::Vars,
    errors::AppError,
    mail::{apply_direction, build_reply_to_for, from_address, present, record_delivery},
    markdown,
    page::PRODUCT_NAME,
    routes::passwords::email_origin,
    state::AppState,
};

const COMPLETED_EMAIL_KEY: &str = "submitter_completed_email";
const DOCUMENTS_COPY_EMAIL_KEY: &str = "submitter_documents_copy_email";
const BCC_EMAILS_KEY: &str = "bcc_emails";
const DOCUMENT_FILENAME_FORMAT_KEY: &str = "document_filename_format";
const RECEIVE_COMPLETED_EMAIL_KEY: &str = "receive_completed_email";
/// `SubmitterMailer::MAX_ATTACHMENTS_SIZE`.
const MAX_ATTACHMENTS_SIZE: i64 = 10 * 1024 * 1024;

/// `User::EMAIL_REGEXP`.
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^@;,<>\s]+@[^@;,<>\s]+").expect("valid regex"));
static PLUS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\+\w+@").expect("valid regex"));

/// `SubmitterMailer.completed_email(submitter, user, to:).deliver_later!`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendCompleted {
    pub submitter_id: i64,
    pub user_id: i64,
    #[serde(default)]
    pub to: Option<String>,
}

impl Job for SendCompleted {
    const KIND: &'static str = "send_submitter_completed_email";
    const QUEUE: &'static str = "mailers";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        send_completed(&state, &self)
            .await
            .map_err(AppError::into_anyhow)
    }
}

/// `SubmitterMailer.documents_copy_email(submitter, to:, sig:).deliver_later!`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendDocumentsCopy {
    pub submitter_id: i64,
    #[serde(default)]
    pub to: Option<String>,
    /// Adds a signed download id to the preview links (`/send_submission_email`).
    #[serde(default)]
    pub sig: bool,
}

impl Job for SendDocumentsCopy {
    const KIND: &'static str = "send_submitter_documents_copy_email";
    const QUEUE: &'static str = "mailers";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        send_documents_copy(&state, &self)
            .await
            .map_err(AppError::into_anyhow)
    }
}

fn is_false(value: Option<&Value>) -> bool {
    value == Some(&Value::Bool(false))
}

/// `enqueue_completed_emails(submitter)`.
pub async fn enqueue_completed_emails(state: &AppState, form: &Form) -> Result<(), AppError> {
    let submitter = &form.submitter;
    let submission = &form.submission;
    let template_preferences = form
        .template
        .as_ref()
        .map_or(Value::Null, |t| t.preferences.0.clone());
    let user = match submission.created_by_user_id {
        Some(id) => users::find(&state.db, id).await?,
        None => match &form.template {
            Some(template) => users::find(&state.db, template.author_id).await?,
            None => None,
        },
    }
    .ok_or_else(|| anyhow::anyhow!("the submission's sender is missing"))?;

    // `AccountConfigs.find_or_initialize_for_key`: the defaults have neither key read here.
    let copy_config = loco_db::submissions::account_config_with_fallback(
        &state.db,
        submitter.account_id,
        DOCUMENTS_COPY_EMAIL_KEY,
    )
    .await?
    .unwrap_or(Value::Null);
    let copy_enabled = !is_false(template_preferences.get("documents_copy_email_enabled"))
        && !is_false(copy_config.get("enabled"));

    if user.account_id == submitter.account_id
        && !is_false(submission.preferences.get("send_email"))
        && !is_false(template_preferences.get("completed_notification_email_enabled"))
    {
        let user_submitter = form
            .submitters
            .iter()
            .find(|s| s.email.as_deref() == Some(user.email.as_str()));
        let is_sent_to_user = user.role != "integration"
            && user.archived_at.is_none()
            && (user_submitter.is_none_or(|s| is_false(s.preferences.get("send_email")))
                || !copy_enabled)
            && !is_false(
                signing::user_config(&state.db, user.id, RECEIVE_COMPLETED_EMAIL_KEY)
                    .await?
                    .as_ref(),
            );
        if is_sent_to_user {
            enqueue(
                state,
                &SendCompleted {
                    submitter_id: submitter.id,
                    user_id: user.id,
                    to: None,
                },
            )
            .await?;
        }

        let mut bcc: Vec<String> = Vec::new();
        for address in EMAIL
            .find_iter(&bcc_completed(state, form, &template_preferences).await?)
            .map(|m| m.as_str().to_owned())
        {
            if !bcc.contains(&address) {
                bcc.push(address);
            }
        }
        for to in bcc {
            if is_sent_to_user && to == user.email {
                continue;
            }
            enqueue(
                state,
                &SendCompleted {
                    submitter_id: submitter.id,
                    user_id: user.id,
                    to: Some(to),
                },
            )
            .await?;
        }
    }

    if copy_enabled {
        let mut recipients: Vec<&Submitter> = form
            .submitters
            .iter()
            .filter(|s| !is_false(s.preferences.get("send_email")))
            .collect();
        // `sort_by { |e| e.completed_at || Time.current }`.
        recipients.sort_by_key(|s| s.completed_at.unwrap_or(chrono::NaiveDateTime::MAX));
        let to: Vec<String> = recipients
            .iter()
            .filter(|s| s.email.as_deref().is_some_and(|e| !e.trim().is_empty()))
            .filter_map(|s| s.friendly_name())
            .collect();
        if !to.is_empty() {
            if copy_config.get("bcc_recipients") == Some(&Value::Bool(true)) {
                for to in to {
                    enqueue(
                        state,
                        &SendDocumentsCopy {
                            submitter_id: submitter.id,
                            to: Some(to),
                            sig: false,
                        },
                    )
                    .await?;
                }
            } else {
                enqueue(
                    state,
                    &SendDocumentsCopy {
                        submitter_id: submitter.id,
                        to: Some(to.join(", ")),
                        sig: false,
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn enqueue<J: Job>(state: &AppState, job: &J) -> Result<(), AppError> {
    loco_jobs::enqueue(&state.db, job, EnqueueOptions::default()).await?;
    Ok(())
}

/// `build_bcc_addresses(submission)`: the request's, the template's, else the account's.
async fn bcc_completed(
    state: &AppState,
    form: &Form,
    template_preferences: &Value,
) -> Result<String, AppError> {
    if let Some(bcc) = present(form.submission.preferences.get("bcc_completed")) {
        return Ok(bcc);
    }
    if let Some(bcc) = present(template_preferences.get("bcc_completed")) {
        return Ok(bcc);
    }
    let rows = signing::account_configs(&state.db, form.account.id, &[BCC_EMAILS_KEY]).await?;
    Ok(rows
        .into_iter()
        .next()
        .map(|(_, v)| crate::api::create::values::ruby_to_s(&v))
        .unwrap_or_default())
}

async fn load_vars(state: &AppState, submitter_id: i64) -> Result<Vars, AppError> {
    let submitter = loco_db::submissions::find_submitter(&state.db, submitter_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("submitter {submitter_id} not found"))?;
    Vars::load(state, submitter)
        .await?
        .ok_or_else(|| anyhow::anyhow!("submission of submitter {submitter_id} not found").into())
}

/// `submitter.template&.preferences || {}`.
fn template_preferences(vars: &Vars) -> Value {
    vars.template
        .as_ref()
        .map_or(Value::Null, |t| t.preferences.0.clone())
}

/// `SubmitterMailer#completed_email`.
async fn send_completed(state: &AppState, job: &SendCompleted) -> Result<(), AppError> {
    let mut vars = load_vars(state, job.submitter_id).await?;
    let user = users::find(&state.db, job.user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user {} not found", job.user_id))?;
    let preferences = template_preferences(&vars);
    let config = loco_db::submissions::account_config_with_fallback(
        &state.db,
        vars.account.id,
        COMPLETED_EMAIL_KEY,
    )
    .await?;
    let config_value = |key: &str| config.as_ref().and_then(|c| c.get(key));

    let attachments = completed_attachments(
        state,
        &mut vars,
        !is_false(config_value("attach_documents"))
            && !is_false(preferences.get("completed_notification_email_attach_documents")),
        !is_false(config_value("attach_audit_log"))
            && !is_false(preferences.get("completed_notification_email_attach_audit")),
    )
    .await?;

    let subject = present(preferences.get("completed_notification_email_subject"))
        .or_else(|| config_value("subject").map(crate::api::create::values::ruby_to_s));
    let body = present(preferences.get("completed_notification_email_body"))
        .or_else(|| present(config_value("body")));

    let locale = vars.account.locale.clone();
    let subject = vars
        .replace(
            state,
            &subject.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
                state.views.t(
                    &locale,
                    "template_name_has_been_completed_by_submitters",
                    &[],
                )
            }),
        )
        .await?;
    let custom_body = match &body {
        Some(body) => Some(markdown::to_html(&vars.replace(state, body).await?)),
        None => None,
    };
    let mut submitters: Vec<&Submitter> = vars.submitters().iter().collect();
    // `order(:completed_at)`: PostgreSQL puts NULLs last.
    submitters.sort_by_key(|s| (s.completed_at.is_none(), s.completed_at, s.id));
    let mut names: Vec<String> = Vec::new();
    for s in submitters {
        let name = s
            .name
            .clone()
            .or_else(|| s.email.clone())
            .or_else(|| s.phone.clone())
            .unwrap_or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let html = apply_direction(state.views.render(
        "submitter_mailer/completed_email.html",
        json!({
            "locale": locale,
            "product_name": PRODUCT_NAME,
            "custom_body": custom_body,
            "name": vars.submission_name(),
            "submitters": names.join(", "),
            "submission_url": format!("{}/submissions/{}", email_origin(state), vars.submission.id),
        }),
    )?);

    let to = job.to.clone().unwrap_or_else(|| {
        if user.role == "integration" {
            PLUS.replace(&user.friendly_name(), "@").into_owned()
        } else {
            user.friendly_name()
        }
    });
    let from = from_address(state, &vars).await?;
    deliver(
        state,
        &vars,
        "submitter_completed",
        Email {
            from,
            to: vec![to],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: None,
            subject,
            html,
            text: None,
            attachments,
            headers: Vec::new(),
        },
    )
    .await
}

/// `SubmitterMailer::SIGN_TTL`.
const SIGN_TTL: chrono::Duration = chrono::Duration::minutes(80);

/// `submitter.signed_id(expires_in: SIGN_TTL, purpose: :download_completed)`.
#[must_use]
pub fn download_sig(state: &AppState, submitter_id: i64) -> String {
    state.secrets.signed_id_verifier.generate(
        &json!(submitter_id),
        Some(&loco_rails_compat::signed_id_purpose(
            "submitter",
            Some("download_completed"),
        )),
        Some(chrono::Utc::now() + SIGN_TTL),
    )
}

/// `SubmitterMailer#documents_copy_email`.
async fn send_documents_copy(state: &AppState, job: &SendDocumentsCopy) -> Result<(), AppError> {
    let mut vars = load_vars(state, job.submitter_id).await?;
    let sig = job.sig.then(|| download_sig(state, job.submitter_id));
    vars.sig.clone_from(&sig);
    let preferences = template_preferences(&vars);
    let config = loco_db::submissions::account_config_with_fallback(
        &state.db,
        vars.account.id,
        DOCUMENTS_COPY_EMAIL_KEY,
    )
    .await?;
    let config_value = |key: &str| config.as_ref().and_then(|c| c.get(key));

    let attachments = completed_attachments(
        state,
        &mut vars,
        !is_false(preferences.get("documents_copy_email_attach_documents"))
            && !is_false(config_value("attach_documents")),
        !is_false(preferences.get("documents_copy_email_attach_audit"))
            && !is_false(config_value("attach_audit_log")),
    )
    .await?;

    let subject = present(preferences.get("documents_copy_email_subject"))
        .or_else(|| config_value("subject").map(crate::api::create::values::ruby_to_s));
    let body = present(preferences.get("documents_copy_email_body"))
        .or_else(|| present(config_value("body")));
    let reply_to = build_reply_to_for(state, &vars, config.as_ref(), true).await?;

    let locale = vars.account.locale.clone();
    let subject = match subject.filter(|s| !s.trim().is_empty()) {
        Some(subject) => vars.replace(state, &subject).await?,
        None => state.views.t(&locale, "your_document_copy", &[]),
    };
    let custom_body = match &body {
        Some(body) => Some(markdown::to_html(&vars.replace(state, body).await?)),
        None => None,
    };
    let html = apply_direction(state.views.render(
        "submitter_mailer/documents_copy_email.html",
        json!({
            "locale": locale,
            "product_name": PRODUCT_NAME,
            "custom_body": custom_body,
            "name": vars.submission_name(),
            "preview_url": crate::email_variables::preview_url(
                &email_origin(state),
                &vars.submission.slug,
                sig.as_deref(),
            ),
            "account_name": vars.account.name,
        }),
    )?);

    let to = job
        .to
        .clone()
        .or_else(|| vars.submitter.friendly_name())
        .unwrap_or_default();
    let from = from_address(state, &vars).await?;
    deliver(
        state,
        &vars,
        "submitter_documents_copy",
        Email {
            from,
            to: vec![to],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to,
            subject,
            html,
            text: None,
            attachments,
            headers: Vec::new(),
        },
    )
    .await
}

async fn deliver(
    state: &AppState,
    vars: &Vars,
    tag: &str,
    mut email: Email,
) -> Result<(), AppError> {
    let message_uuid = uuid::Uuid::new_v4().to_string();
    email
        .headers
        .push(("X-Message-Uuid".to_owned(), message_uuid.clone()));
    let delivered = state.mailer.deliver(&email).await?;
    record_delivery(
        state,
        vars.submission.account_id,
        tag,
        &message_uuid,
        vars.submitter.id,
        &delivered,
    )
    .await;
    Ok(())
}

/// The account's `document_filename_format`, when present.
pub(super) async fn filename_format(
    state: &AppState,
    account_id: i64,
) -> Result<Option<String>, AppError> {
    Ok(
        signing::account_configs(&state.db, account_id, &[DOCUMENT_FILENAME_FORMAT_KEY])
            .await?
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .filter(|v| present(Some(v)).is_some())
            .map(|v| crate::api::create::values::ruby_to_s(&v)),
    )
}

/// `add_completed_email_attachments!(submitter, with_audit_log:, with_documents:)`: the
/// result documents, the audit trail, and the uploaded files when one party has file
/// fields, while the total stays under `MAX_ATTACHMENTS_SIZE`.
async fn completed_attachments(
    state: &AppState,
    vars: &mut Vars,
    with_documents: bool,
    with_audit_log: bool,
) -> Result<Vec<EmailAttachment>, AppError> {
    let documents = if with_documents {
        select_completed_documents(state, vars).await?
    } else {
        Vec::new()
    };
    let filename_format = filename_format(state, vars.submitter.account_id).await?;

    let mut out = Vec::new();
    let mut total_size = 0;
    let audit = if with_audit_log
        && documents
            .first()
            .is_none_or(|(a, _)| a.name != "combined_document")
    {
        Attachment::with_blobs_for_records(
            &state.db,
            "Submission",
            &[vars.submission.id],
            "audit_trail",
        )
        .await?
        .into_iter()
        .next()
    } else {
        None
    };
    let audit = match audit {
        Some((_, blob)) => {
            let data = state.storage.download(&blob).await?;
            total_size = i64::try_from(data.len()).unwrap_or(i64::MAX);
            Some((blob, data))
        }
        None => None,
    };

    total_size = add_with_size_limit(
        state,
        vars,
        &mut out,
        documents.iter().map(|(_, b)| b),
        total_size,
        filename_format.as_deref(),
    )
    .await?;

    if let Some((blob, data)) = audit {
        let filename = document_filename(state, vars, &blob, filename_format.as_deref()).await?;
        out.push(attachment(filename.replace('"', "'"), data.to_vec()));
    }

    if with_documents {
        let file_fields: Vec<&Value> = vars
            .fields()
            .iter()
            .filter(|f| {
                matches!(
                    f.get("type").and_then(Value::as_str),
                    Some("file" | "payment")
                )
            })
            .collect();
        let mut owners: Vec<&str> = file_fields
            .iter()
            .map(|f| {
                f.get("submitter_uuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            })
            .collect();
        owners.dedup();
        owners.sort_unstable();
        owners.dedup();
        if owners.len() == 1 {
            let uuids: Vec<String> = file_fields
                .iter()
                .filter_map(|f| f.get("uuid").and_then(Value::as_str))
                .filter_map(|u| vars.submitter.values.get(u))
                .flat_map(|v| super::results::wrap(Some(v)))
                .map(|v| crate::api::create::values::ruby_to_s(&v))
                .collect();
            let uploads = Attachment::with_blobs_for_records(
                &state.db,
                "Submitter",
                &[vars.submitter.id],
                "attachments",
            )
            .await?;
            let blobs: Vec<Blob> = uploads
                .into_iter()
                .filter(|(a, _)| uuids.contains(&a.uuid))
                .map(|(_, b)| b)
                .collect();
            add_with_size_limit(state, vars, &mut out, blobs.iter(), total_size, None).await?;
        }
    }
    Ok(out)
}

/// `select_completed_documents(submitter)`: the last party's result documents.
async fn select_completed_documents(
    state: &AppState,
    vars: &Vars,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    let Some(last) = vars
        .submitters()
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| (s.completed_at, s.id))
        .cloned()
    else {
        return Ok(Vec::new());
    };
    super::results::ensure_result_generated(state, &last).await?;
    let form = Form::for_submitter(state, last).await?;
    super::combined::select_attachments_for_download(state, &form).await
}

async fn add_with_size_limit<'a>(
    state: &AppState,
    vars: &mut Vars,
    out: &mut Vec<EmailAttachment>,
    blobs: impl Iterator<Item = &'a Blob>,
    current_size: i64,
    filename_format: Option<&str>,
) -> Result<i64, AppError> {
    let mut total_size = current_size;
    for blob in blobs {
        total_size += blob.byte_size;
        if total_size >= MAX_ATTACHMENTS_SIZE {
            break;
        }
        let filename = document_filename(state, vars, blob, filename_format).await?;
        let data = state.storage.download(blob).await?;
        out.push(attachment(filename.replace('"', "'"), data.to_vec()));
    }
    Ok(total_size)
}

/// `attachments[filename] = data`: the type follows the extension.
fn attachment(filename: String, data: Vec<u8>) -> EmailAttachment {
    let content_type = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    EmailAttachment {
        filename,
        content_type,
        data,
    }
}

/// `ActiveStorage::Filename#base` and `#extension_without_delimiter`.
fn split_filename(filename: &str) -> (&str, &str) {
    match filename.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() => (base, extension),
        _ => (filename, ""),
    }
}

/// `Submitters.build_document_filename(submitter, blob, filename_format)`.
pub(super) async fn document_filename(
    state: &AppState,
    vars: &mut Vars,
    blob: &Blob,
    filename_format: Option<&str>,
) -> Result<String, AppError> {
    let Some(format) = filename_format else {
        return Ok(blob.filename.clone());
    };
    let (base, extension) = split_filename(&blob.filename);
    let filename = format.replace("{document.name}", base);
    let mut filename = vars.replace(state, &filename).await?;
    let locale = vars.account.locale.clone();
    if filename.contains(" - {submission.status}") {
        let status = if vars.submission.completed_at.is_some() {
            let key = if vars
                .fields()
                .iter()
                .any(|f| f.get("type").and_then(Value::as_str) == Some("signature"))
            {
                "signed"
            } else {
                "completed"
            };
            format!(" - {}", state.views.t(&locale, key, &[]))
        } else {
            String::new()
        };
        filename = filename.replace(" - {submission.status}", &status);
    }
    if filename.contains("{submission.completed_at}") {
        let completed_at = vars.submission.completed_at.or_else(|| {
            vars.submitters()
                .iter()
                .filter_map(|s| s.completed_at)
                .max()
        });
        let text = completed_at
            .map(|at| crate::l10n::time(state, &locale, at, &vars.account.timezone, "short"))
            .unwrap_or_default();
        filename = filename.replace("{submission.completed_at}", &text);
    }
    Ok(format!("{filename}.{extension}").replace('/', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_filenames_like_active_storage() {
        assert_eq!(split_filename("contract.pdf"), ("contract", "pdf"));
        assert_eq!(split_filename("a.b.pdf"), ("a.b", "pdf"));
        assert_eq!(split_filename(".bashrc"), (".bashrc", ""));
        assert_eq!(split_filename("plain"), ("plain", ""));
    }
}
