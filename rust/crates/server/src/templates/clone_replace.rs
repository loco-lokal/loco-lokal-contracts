//! `TemplatesCloneAndReplaceController#create` (`POST /templates/:template_id/clone_and_replace`):
//! a copy of the template (`Templates::Clone`) whose first documents are the uploaded
//! files (`Templates::ReplaceAttachments`); the field areas of a replaced document move
//! to its replacement, and the documents that were not replaced are copied
//! (`Templates::CloneAttachments`).

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use loco_db::{
    JsonText,
    templates::{self, NewTemplate},
};
use loco_domain::ability::Action;
use loco_storage::Attachment;
use serde_json::{Value, json};

use super::{CreateError, basename, create_attachments, rack_filename, uploads};
use crate::{
    api::{self, params::FormParams},
    auth::RequireAuth,
    errors::AppError,
    session::Session,
    signing::actions::redirect_to,
    state::AppState,
};

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("application/json"))
}

fn area_uuids(field: &Value) -> impl Iterator<Item = &str> {
    field
        .get("areas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("attachment_uuid").and_then(Value::as_str))
}

/// `Templates::ReplaceAttachments` after `CreateAttachments`: the new documents take the
/// first schema places and the areas of the documents they replace; a document whose
/// pages no field uses brings its form fields, for the first party.
fn replace_attachments(
    schema: &mut Vec<Value>,
    fields: &mut Vec<Value>,
    first_party: Option<&str>,
    documents: &mut [super::Document],
) {
    for (index, document) in documents.iter_mut().enumerate() {
        let replaced = schema.get(index).cloned();
        let item = json!({
            "attachment_uuid": document.attachment.uuid,
            "name": basename(&document.blob.filename),
        });
        if index < schema.len() {
            schema[index] = item;
        } else {
            schema.push(item);
        }
        let new_uuid = document.attachment.uuid.clone();
        if let Some(old) = replaced
            .as_ref()
            .and_then(|r| r.get("attachment_uuid"))
            .and_then(Value::as_str)
        {
            for area in fields
                .iter_mut()
                .filter_map(|f| f.get_mut("areas").and_then(Value::as_array_mut))
                .flatten()
            {
                if area.get("attachment_uuid").and_then(Value::as_str) == Some(old) {
                    area["attachment_uuid"] = new_uuid.clone().into();
                }
            }
        }

        if fields.iter().any(|f| area_uuids(f).any(|u| u == new_uuid)) {
            continue;
        }
        let Some(party) = first_party else { continue };
        let Some(pdf) = document
            .metadata
            .get_mut("pdf")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        if pdf
            .get("fields")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            continue;
        }
        let Some(Value::Array(mut pdf_fields)) = pdf.shift_remove("fields") else {
            continue;
        };
        for field in &mut pdf_fields {
            field["submitter_uuid"] = party.into();
        }
        if index > 0 {
            let previous = schema[index - 1]
                .get("attachment_uuid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            // Rails inserts at the document's index, not after the previous one's fields.
            if fields.iter().any(|f| area_uuids(f).any(|u| u == previous)) {
                let at = index.min(fields.len());
                fields.splice(at..at, pdf_fields);
            } else {
                fields.extend(pdf_fields);
            }
        } else {
            fields.extend(pdf_fields);
            schema[index]["pending_fields"] = true.into();
        }
    }
}

/// `POST /templates/:template_id/clone_and_replace`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    form: FormParams,
) -> Result<Response, AppError> {
    let original =
        match uploads::authorized_template(&state, &auth, &session, &headers, &template_id, &[])
            .await?
        {
            Ok(template) => template,
            Err(response) => return Ok(response),
        };
    let files: Vec<_> = form
        .files
        .iter()
        .filter(|(name, _)| name == "files[]" || name == "files")
        .map(|(_, file)| file)
        .collect();
    let Some(first) = files.first() else {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    };
    if let Some(message) = super::unsupported(&files) {
        return Ok(api::unprocessable(message));
    }
    let original_schema = original.schema.as_array().cloned().unwrap_or_default();
    if original_schema.iter().any(|i| {
        !matches!(
            i.get("dynamic"),
            None | Some(Value::Null | Value::Bool(false))
        )
    }) {
        return Err(AppError::Unsupported(
            "Cloning dynamic documents is not supported yet".into(),
        ));
    }
    // `authorize!(:create, cloned_template)`: a template of the user's account.
    if !auth
        .ability()
        .can_template(Action::Create, Some(auth.account.id), &[])
    {
        session.flash_alert("You are not authorized to access this page.");
        return Ok(redirect_to(&state, &headers, "/"));
    }

    // `Templates::Clone.call(@template, author: current_user)`, named after the first file.
    let mut submitters = original.submitters.0.clone();
    let mut fields = original.fields.0.clone();
    let mut schema = original.schema.0.clone();
    let mut preferences = original.preferences.0.clone();
    api::templates::renew_uuids(&mut submitters, &mut fields, &mut schema, &mut preferences);
    let folder_id = if auth.account.id == original.account_id {
        original.folder_id
    } else {
        templates::default_folder(&state.db, auth.account.id).await?
    };
    let template = templates::insert(
        &state.db,
        &NewTemplate {
            name: basename(&rack_filename(&first.filename)),
            slug: loco_rails_compat::base58(14),
            schema: JsonText(schema.clone()),
            fields: JsonText(fields.clone()),
            submitters: JsonText(submitters.clone()),
            author_id: auth.user.id,
            account_id: auth.account.id,
            source: "native".into(),
            folder_id,
            external_id: None,
            preferences: JsonText(preferences),
            shared_link: original.shared_link,
            variables_schema: original.variables_schema.clone(),
        },
    )
    .await?;
    templates::copy_accesses(&state.db, original.id, template.id).await?;

    let password = form.params.str("password");
    let mut documents =
        match create_attachments(&state, template.id, &files, password.as_deref(), true).await {
            Ok(documents) => documents,
            Err(CreateError::PdfEncrypted) => {
                if wants_json(&headers) {
                    return Ok(api::render(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        &json!({ "error": "PDF encrypted", "status": "pdf_encrypted" }),
                    ));
                }
                // `render turbo_stream:` in the `f.html` block keeps the HTML type.
                let mut response =
                    uploads::prompt_password(&form.params.str("form_id").unwrap_or_default());
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/html; charset=utf-8"),
                );
                return Ok(response);
            }
            Err(CreateError::Failed(err)) => return Err(AppError::Internal(err)),
        };

    let mut schema = schema.as_array().cloned().unwrap_or_default();
    let mut fields = fields.as_array().cloned().unwrap_or_default();
    let first_party = submitters
        .get(0)
        .and_then(|s| s.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    replace_attachments(
        &mut schema,
        &mut fields,
        first_party.as_deref(),
        &mut documents,
    );

    // `Templates::CloneAttachments` with the new documents excluded.
    let excluded: Vec<&str> = documents
        .iter()
        .map(|d| d.attachment.uuid.as_str())
        .collect();
    let mut replacements: Vec<(String, String)> = Vec::new();
    for item in &mut schema {
        let Some(old) = item.get("attachment_uuid").and_then(Value::as_str) else {
            continue;
        };
        if excluded.contains(&old) {
            continue;
        }
        let new = uuid::Uuid::new_v4().to_string();
        replacements.push((old.to_owned(), new.clone()));
        item["attachment_uuid"] = new.into();
    }
    for area in fields
        .iter_mut()
        .filter_map(|f| f.get_mut("areas").and_then(Value::as_array_mut))
        .flatten()
    {
        let new = area
            .get("attachment_uuid")
            .and_then(Value::as_str)
            .and_then(|old| replacements.iter().find(|(o, _)| o == old))
            .map(|(_, new)| new.clone());
        if let Some(new) = new {
            area["attachment_uuid"] = new.into();
        }
    }
    let originals =
        Attachment::with_blobs_for_records(&state.db, "Template", &[original.id], "documents")
            .await?;
    let previews = Attachment::with_blobs_for_records(
        &state.db,
        "ActiveStorage::Attachment",
        &originals.iter().map(|(a, _)| a.id).collect::<Vec<_>>(),
        super::PREVIEW_IMAGES,
    )
    .await?;
    let schema_uuids: Vec<&str> = original_schema
        .iter()
        .filter_map(|i| i.get("attachment_uuid").and_then(Value::as_str))
        .collect();
    let mut tx = state.db.begin().await?;
    templates::update_schema_and_fields(
        &mut *tx,
        template.id,
        &JsonText(Value::Array(schema)),
        &JsonText(Value::Array(fields)),
    )
    .await?;
    for (attachment, _) in originals
        .iter()
        .filter(|(a, _)| schema_uuids.contains(&a.uuid.as_str()))
    {
        let Some((_, new_uuid)) = replacements.iter().find(|(old, _)| *old == attachment.uuid)
        else {
            continue;
        };
        let copy = Attachment::create_for_blob(
            &mut *tx,
            "Template",
            template.id,
            "documents",
            attachment.blob_id,
            new_uuid,
        )
        .await?;
        for (preview, _) in previews
            .iter()
            .filter(|(p, _)| p.record_id == attachment.id)
        {
            Attachment::create_for_blob(
                &mut *tx,
                "ActiveStorage::Attachment",
                copy.id,
                super::PREVIEW_IMAGES,
                preview.blob_id,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        }
    }
    tx.commit().await?;

    crate::search::reindex_later(&state, "Template", &[template.id]).await;

    if wants_json(&headers) {
        return Ok(api::render(StatusCode::OK, &json!({ "id": template.id })));
    }
    Ok(redirect_to(
        &state,
        &headers,
        &format!("/templates/{}/edit", template.id),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_areas_to_the_replacing_document() {
        let mut schema = vec![
            json!({ "attachment_uuid": "old-a", "name": "A" }),
            json!({ "attachment_uuid": "old-b", "name": "B" }),
        ];
        let mut fields = vec![json!({ "uuid": "f", "areas": [{ "attachment_uuid": "old-a" }] })];
        replace_attachments(&mut schema, &mut fields, Some("p"), &mut []);
        assert_eq!(schema[0]["attachment_uuid"], "old-a");
        assert_eq!(fields[0]["areas"][0]["attachment_uuid"], "old-a");
    }
}
