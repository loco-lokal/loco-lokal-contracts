//! Result document links: `SubmitFormDownloadController` (`/s/:slug/download`),
//! `SubmitFormCompletedDownloadController` (`/s/:slug/documents` and
//! `/submitters/:slug/download`) and `SubmittersDownloadController`
//! (`/submitters/:id/download`), with `Submitters.build_document_urls`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use loco_db::{signing, submissions::Submitter};
use loco_storage::{Attachment, Blob};
use serde_json::Value;

use super::{Access, Form, actions::redirect_to, keys};
use crate::{
    api::Params,
    auth::{MaybeAuth, RequireAuth},
    email_variables::Vars,
    errors::AppError,
    session::Session,
    state::AppState,
};

/// `FILES_TTL`: how long the returned links stay valid.
const FILES_TTL: Duration = Duration::minutes(5);
/// `SubmitFormCompletedDownloadController::TTL`: how long after completion an
/// unauthenticated party may still download.
const COMPLETED_TTL: Duration = Duration::minutes(40);

fn head(status: StatusCode) -> Response {
    status.into_response()
}

/// 422 for the combined result document, which is not ported.
fn combined_unsupported() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(serde_json::json!({
            "error": "Combining the result documents into one PDF is not supported yet"
        })),
    )
        .into_response()
}

/// `select_attachments_for_download` uses the combined document for completed
/// submissions without verification fields when the account combines results.
async fn uses_combined(state: &AppState, form: &Form) -> Result<bool, AppError> {
    if form.submission.completed_at.is_none()
        || form
            .fields()
            .iter()
            .any(|f| f.get("type").and_then(Value::as_str) == Some("verification"))
    {
        return Ok(false);
    }
    Ok(super::audit::check_combined(state, form.account.id)
        .await
        .is_err())
}

/// `ActiveStorage::Blob.proxy_path(blob, expires_at: FILES_TTL.from_now.to_i, filename:)`.
async fn proxy_path(
    state: &AppState,
    blob: &mut Blob,
    filename: Option<&str>,
) -> Result<String, AppError> {
    let uuid = blob.ensure_uuid(&state.db).await?;
    let filename = filename.map_or_else(|| blob.filename.clone(), str::to_owned);
    // Rails signs `expires_at.to_i`, so drop the sub-second part.
    let expires_at = Utc::now() + FILES_TTL;
    Ok(crate::files::path_for(
        state,
        &uuid,
        &filename,
        Some(expires_at),
    ))
}

/// `Submitters.select_attachments_for_download(submitter)`, rejecting combined results
/// where Rails would use them (completed submission, no verification fields).
async fn select_for_download(
    state: &AppState,
    submitter: Submitter,
) -> Result<(Form, Vec<(Attachment, Blob)>), AppError> {
    let form = Form::for_submitter(state, submitter).await?;
    let attachments = super::audit::select_attachments(state, &form).await?;
    Ok((form, attachments))
}

/// `Submitters.build_document_urls(submitter)`.
async fn build_document_urls(state: &AppState, submitter: Submitter) -> Result<Value, AppError> {
    let (_, attachments) = select_for_download(state, submitter.clone()).await?;
    let filename_format = super::emails::filename_format(state, submitter.account_id).await?;
    let mut vars = Vars::load(state, submitter)
        .await?
        .ok_or_else(|| anyhow::anyhow!("submitter without submission"))?;
    let mut urls = Vec::with_capacity(attachments.len());
    for (_, mut blob) in attachments {
        let filename =
            super::emails::document_filename(state, &mut vars, &blob, filename_format.as_deref())
                .await?;
        urls.push(Value::String(
            proxy_path(state, &mut blob, Some(&filename)).await?,
        ));
    }
    Ok(Value::Array(urls))
}

/// `GET /s/:slug/download`: links to the documents as signed so far.
pub async fn partial(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if form.submitter.completed_at.is_some() {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/s/{}/documents", form.submitter.slug),
        ));
    }
    let partial_download_off = signing::account_configs(
        &state.db,
        form.submitter.account_id,
        &[keys::ALLOW_TO_PARTIAL_DOWNLOAD],
    )
    .await?
    .iter()
    .any(|(_, v)| *v == Value::Bool(false));
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if form.submitter.declined_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.submission.completed_at.is_some()
        || form.template_archived()
        || partial_download_off
        || !access.authorized(&form)
        || !access.pass_submitters_order(&form).await?
    {
        return Ok(head(StatusCode::UNPROCESSABLE_ENTITY));
    }

    // `max_by(&:completed_at)` keeps the first of equal times.
    let mut last_completed: Option<&Submitter> = None;
    for s in &form.submitters {
        if s.id == form.submitter.id {
            continue;
        }
        if let Some(at) = s.completed_at
            && last_completed.is_none_or(|l| l.completed_at.is_some_and(|l| at > l))
        {
            last_completed = Some(s);
        }
    }

    let mut blobs: Vec<Blob> = if let Some(last) = last_completed.cloned() {
        let (_, attachments) = select_for_download(&state, last).await?;
        attachments.into_iter().map(|(_, b)| b).collect()
    } else {
        super::pages::schema_documents(&state, &form)
            .await?
            .unwrap_or_default()
            .into_iter()
            .map(|d| d.blob)
            .collect()
    };
    let mut urls = Vec::with_capacity(blobs.len());
    for blob in &mut blobs {
        urls.push(Value::String(proxy_path(&state, blob, None).await?));
    }
    Ok(crate::api::ok(&Value::Array(urls)))
}

/// `GET /s/:slug/documents` and `GET /submitters/:slug/download`.
pub async fn completed(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    completed_for(&state, &slug, auth.as_ref(), &headers, &params).await
}

async fn completed_for(
    state: &AppState,
    slug: &str,
    auth: Option<&crate::auth::Auth>,
    headers: &HeaderMap,
    params: &Params,
) -> Result<Response, AppError> {
    let mut signed = None;
    if let Some(sig) = params.str("sig") {
        let purpose = loco_rails_compat::signed_id_purpose("submitter", Some("download_completed"));
        if let Some(id) = state
            .secrets
            .signed_id_verifier
            .verify(&sig, Some(&purpose))
            .ok()
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        {
            signed = loco_db::submissions::find_submitter(&state.db, id).await?;
        }
    }
    let signature_valid = signed.as_ref().is_some_and(|s| s.slug == slug);
    let submitter = match signed.filter(|_| signature_valid) {
        Some(s) => s,
        None => loco_db::submissions::find_submitter_by_slug(&state.db, slug)
            .await?
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?,
    };
    let form = Form::for_submitter(state, submitter).await?;
    let completed = form.submitter.completed_at.is_some()
        || (form.is_viewer(&form.submitter) && form.submission.completed_at.is_some());
    if !completed {
        return Ok(head(StatusCode::NOT_FOUND));
    }
    if form.submitter.completed_at.is_some() {
        super::results::ensure_result_generated(state, &form.submitter).await?;
    }
    // `submitters.completed.order(:completed_at).last`
    let Some(last) = form
        .submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at)
        .cloned()
    else {
        return Ok(head(StatusCode::NOT_FOUND));
    };
    super::results::ensure_result_generated(state, &last).await?;

    let current_user_can_read =
        auth.is_some_and(|a| a.ability().can_account_record(last.account_id));
    if !signature_valid && !current_user_can_read {
        let access = Access {
            state,
            headers,
            params: &params.0,
            current_user: auth.map(|a| &a.user),
        };
        if !access.authorized(&form) || !access.pass_submitters_order(&form).await? {
            return Ok(head(StatusCode::NOT_FOUND));
        }
        if last
            .completed_at
            .is_some_and(|at| at < (Utc::now() - COMPLETED_TTL).naive_utc())
        {
            return Ok(head(StatusCode::NOT_FOUND));
        }
    }

    let last_form = Form::for_submitter(state, last.clone()).await?;
    if params.str("combined").as_deref() == Some("true") {
        // `build_combined_url` is nil until the submission completes.
        if last_form.submission.completed_at.is_none() {
            return Ok(head(StatusCode::NOT_FOUND));
        }
        return Ok(combined_unsupported());
    }
    if uses_combined(state, &last_form).await? {
        return Ok(combined_unsupported());
    }
    let urls = build_document_urls(state, last).await?;
    Ok(crate::api::ok(&urls))
}

/// `GET /submitters/:key/download`: by id for signed-in users
/// (`SubmittersDownloadController`, `submitter_id: /\d+/`), otherwise by slug.
pub async fn submitter(
    State(state): State<AppState>,
    Path(key): Path<String>,
    MaybeAuth(maybe_auth): MaybeAuth,
    required: Result<RequireAuth, Response>,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
        return completed_for(&state, &key, maybe_auth.as_ref(), &headers, &params).await;
    }
    let auth = match required {
        Ok(RequireAuth(auth)) => auth,
        Err(response) => return Ok(response),
    };
    let id: i64 = key
        .parse()
        .map_err(|_| AppError::Status(StatusCode::NOT_FOUND))?;
    let Some(submitter) = loco_db::submissions::find_submitter(&state.db, id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if !auth.ability().can_account_record(submitter.account_id) {
        session.flash_alert("You are not authorized to access this page.");
        return Ok(redirect_to(&state, &headers, "/"));
    }
    super::results::ensure_result_generated(&state, &submitter).await?;
    if uses_combined(
        &state,
        &Form::for_submitter(&state, submitter.clone()).await?,
    )
    .await?
    {
        return Ok(combined_unsupported());
    }
    let urls = build_document_urls(&state, submitter).await?;
    Ok(crate::api::ok(&urls))
}
