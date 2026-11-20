//! `TemplateDocumentsController#index` (`GET /templates/:template_id/documents`) and
//! `TemplateDocumentsCropController` (`/templates/:template_id/documents_crop`).
//!
//! Cropping is not supported yet: Rails detects the page corners, warps, turns and
//! dewarps the photo with Leptonica, and resizes and decodes it with libvips. Both
//! endpoints answer 422 once the template and the attachment are found.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{Duration, Utc};
use loco_domain::ability::Action;
use loco_storage::Attachment;
use serde::Deserialize;
use serde_json::Value;

use super::uploads::authorized_template;
use crate::{
    api::{self, params::Params},
    auth::RequireAuth,
    errors::AppError,
    session::Session,
    state::AppState,
};

/// `TemplateDocumentsController::FILES_TTL`.
const FILES_TTL_MINUTES: i64 = 5;
const CROP_UNSUPPORTED: &str = "Cropping documents is not supported yet";

/// `GET /templates/:template_id/documents`: the proxy paths of the schema documents, in
/// schema order, valid for five minutes.
pub async fn index(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
) -> Result<Response, AppError> {
    let template =
        match authorized_template(&state, &auth, &session, &headers, &template_id, &[]).await? {
            Ok(template) => template,
            Err(response) => return Ok(response),
        };
    let schema = template.schema.as_array().cloned().unwrap_or_default();
    if schema.iter().any(|item| {
        !matches!(
            item.get("dynamic"),
            None | Some(Value::Null | Value::Bool(false))
        )
    }) {
        return Err(AppError::Unsupported(
            "Dynamic documents are not supported yet".into(),
        ));
    }
    let mut documents =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template.id], "documents")
            .await?;
    let expires_at = Utc::now() + Duration::minutes(FILES_TTL_MINUTES);
    let mut paths = Vec::new();
    for item in &schema {
        let uuid = item.get("attachment_uuid").and_then(Value::as_str);
        let Some((_, blob)) = documents
            .iter_mut()
            .find(|(a, _)| Some(a.uuid.as_str()) == uuid)
        else {
            return Err(AppError::Internal(anyhow::anyhow!(
                "undefined method 'blob' for nil"
            )));
        };
        let blob_uuid = blob.ensure_uuid(&state.db).await?;
        paths.push(Value::String(crate::files::path_for(
            &state,
            &blob_uuid,
            &blob.filename,
            Some(expires_at),
        )));
    }
    Ok(api::render(StatusCode::OK, &Value::Array(paths)))
}

#[derive(Deserialize)]
pub struct CropQuery {
    attachment_uuid: Option<String>,
}

/// `load_attachment`: the template's document, or 404.
async fn crop_attachment(
    state: &AppState,
    template_id: i64,
    uuid: Option<&str>,
) -> Result<(), AppError> {
    let found =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template_id], "documents")
            .await?
            .iter()
            .any(|(a, _)| Some(a.uuid.as_str()) == uuid);
    if found {
        Ok(())
    } else {
        Err(AppError::Status(StatusCode::NOT_FOUND))
    }
}

/// `GET /templates/:template_id/documents_crop?attachment_uuid=`: the page corners
/// Leptonica finds (not supported yet).
pub async fn crop_corners(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    Query(query): Query<CropQuery>,
) -> Result<Response, AppError> {
    let template =
        match authorized_template(&state, &auth, &session, &headers, &template_id, &[]).await? {
            Ok(template) => template,
            Err(response) => return Ok(response),
        };
    crop_attachment(&state, template.id, query.attachment_uuid.as_deref()).await?;
    Err(AppError::Unsupported(CROP_UNSUPPORTED.into()))
}

/// `POST /templates/:template_id/documents_crop`: a cropped copy of a photographed page
/// (not supported yet).
pub async fn crop(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let template =
        match authorized_template(&state, &auth, &session, &headers, &template_id, &[]).await? {
            Ok(template) => template,
            Err(response) => return Ok(response),
        };
    // `before_action :load_attachment` runs before `authorize!(:update, @template)`.
    crop_attachment(
        &state,
        template.id,
        params.str("attachment_uuid").as_deref(),
    )
    .await?;
    if !api::templates::can(&state, &auth, &template, Action::Update).await? {
        session.flash_alert("You are not authorized to access this page.");
        return Ok(crate::signing::actions::redirect_to(&state, &headers, "/"));
    }
    Err(AppError::Unsupported(CROP_UNSUPPORTED.into()))
}
