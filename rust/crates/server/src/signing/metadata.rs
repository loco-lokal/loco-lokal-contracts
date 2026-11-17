//! `SubmitFormMetadataController`: the text runs of the form's documents (the
//! accessibility layer over the page images), cached per blob checksum in
//! `document_metadata` (`DocumentMetadatas.find_or_create_for_document`).

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use loco_db::signing;
use loco_domain::submit as rules;
use serde_json::{Map, Value, json};

use super::{Access, Form, pages};
use crate::{api::Params, auth::MaybeAuth, errors::AppError, state::AppState};

/// `DocumentMetadatas.build_text_runs`.
fn build_text_runs(bytes: &[u8]) -> Result<Value, AppError> {
    let pages = loco_pdf::with_shared(|pdfium| loco_pdf::text::text_objects(pdfium, bytes))??;
    let mut runs = Map::new();
    for (index, objects) in pages.into_iter().enumerate() {
        let items = objects
            .into_iter()
            .map(|o| {
                json!({
                    "text": o.content,
                    "x": o.x,
                    "y": o.y,
                    "w": o.w,
                    "h": o.h,
                    "font_size": match o.font_size {
                        loco_pdf::text::FontSize::Float(size) => json!(size),
                        loco_pdf::text::FontSize::Eight => json!(8),
                    },
                })
            })
            .collect();
        runs.insert(index.to_string(), Value::Array(items));
    }
    Ok(Value::Object(runs))
}

/// `DocumentMetadatas.find_or_create_for_document(document, account_id:).text_runs`.
async fn text_runs(
    state: &AppState,
    account_id: i64,
    document: &pages::Document,
) -> Result<Value, AppError> {
    let checksum = document
        .blob
        .checksum
        .clone()
        .ok_or_else(|| anyhow::anyhow!("blob {} has no checksum", document.blob.id))?;
    if let Some(text) =
        signing::document_metadata_text_runs(&state.db, account_id, &checksum).await?
    {
        return Ok(serde_json::from_str(&text)?);
    }
    let pages = document
        .blob
        .metadata
        .as_deref()
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|m| m.pointer("/pdf/number_of_pages").cloned())
        .map_or(0, |n| {
            crate::api::params::to_i(&crate::api::create::values::ruby_to_s(&n))
        });
    let runs = if pages == 0 {
        json!({})
    } else {
        let bytes = state.storage.download(&document.blob).await?;
        tokio::task::spawn_blocking(move || build_text_runs(&bytes)).await??
    };
    signing::insert_document_metadata(
        &state.db,
        account_id,
        &checksum,
        &crate::api::ruby_json(&runs),
    )
    .await?;
    match signing::document_metadata_text_runs(&state.db, account_id, &checksum).await? {
        Some(text) => Ok(serde_json::from_str(&text)?),
        None => Ok(runs),
    }
}

/// `GET /s/:slug/metadata`.
pub async fn index(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    // `authorized_submitter?`
    if form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.submission.completed_at.is_some()
        || form.expired()
        || form.template_archived()
        || form.account.archived_at.is_some()
        || !access.authorized(&form)
        || !access.pass_submitters_order(&form).await?
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }

    let Some(documents) = pages::schema_documents(&state, &form).await? else {
        return Ok(crate::api::unprocessable(
            "Templates with dynamic documents are not supported yet",
        ));
    };
    let values = form.all_values();
    let schema = rules::filtered_conditions_schema(
        form.schema(),
        form.fields(),
        &values,
        Some(&form.submitter.uuid),
        &form.option_word(&state),
    );
    let mut runs = Map::new();
    for item in &schema {
        let uuid = item.get("attachment_uuid").and_then(Value::as_str);
        let Some(document) = documents
            .iter()
            .find(|d| Some(d.attachment.uuid.as_str()) == uuid)
        else {
            continue;
        };
        let value = text_runs(&state, form.submission.account_id, document).await?;
        runs.insert(document.attachment.uuid.clone(), value);
    }
    Ok(crate::api::render_ruby_floats(
        StatusCode::OK,
        &json!({ "text_runs": runs }),
    ))
}
