//! `SubmitFormPrefillAttachmentsController`: attaches a prefilled signature or initials
//! (the signed `prefill_token` of `Submitters.build_prefill_attachment_data`) to the
//! submitter, so the form can use it as the field's value.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};

use super::{
    Form,
    actions::{attachment_json, request_origin},
};
use crate::{api::Params, auth::MaybeAuth, errors::AppError, locale, state::AppState};

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

/// `POST /s/:slug/prefill_attachments`.
pub async fn create(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    // `can_upload?`
    if form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.template_archived()
    {
        let locale = locale::app_locale(&state, auth.as_ref(), None);
        return Ok(crate::api::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({ "error": state.views.t(&locale, "form_has_been_archived", &[]) }),
        ));
    }

    // `signed_id_verifier.verify(params[:prefill_token], purpose: :prefill_attachment)`
    // raises on a bad token.
    let token = params.str("prefill_token").unwrap_or_default();
    let verified = state
        .secrets
        .signed_id_verifier
        .verify(&token, Some("prefill_attachment"))
        .map_err(|_| anyhow::anyhow!("ActiveSupport::MessageVerifier::InvalidSignature"))?;
    let (uuid, slug) = match verified.as_array().map(Vec::as_slice) {
        Some([uuid, slug, ..]) => (uuid.clone(), slug.clone()),
        Some([uuid]) => (uuid.clone(), Value::Null),
        _ => (Value::Null, Value::Null),
    };
    if slug.as_str() != Some(form.submitter.slug.as_str()) {
        return Err(not_found());
    }
    let Some(source) = uuid.as_str() else {
        return Err(not_found());
    };
    let source = Attachment::find_by_uuid(&state.db, source)
        .await?
        .ok_or_else(not_found)?;

    // `attachments_attachments.find_or_create_by!(blob_id:)`
    let existing = Attachment::for_record(&state.db, "Submitter", form.submitter.id, "attachments")
        .await?
        .into_iter()
        .find(|a| a.blob_id == source.blob_id);
    let mut blob = Blob::find(&state.db, source.blob_id)
        .await?
        .ok_or_else(not_found)?;
    let attachment = match existing {
        Some(attachment) => attachment,
        None => {
            Attachment::create(
                &state.db,
                "Submitter",
                form.submitter.id,
                "attachments",
                &blob,
            )
            .await?
        }
    };
    blob.ensure_uuid(&state.db).await?;
    let origin = request_origin(&state, &headers);
    Ok(crate::api::ok(&attachment_json(
        &state,
        &origin,
        &attachment,
        &blob,
    )))
}
