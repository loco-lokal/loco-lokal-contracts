//! `TemplatesUploadsController#create` (`POST /templates_upload`) and
//! `TemplateDocumentsController#create` (`POST /templates/:template_id/documents`).

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use loco_db::{
    JsonText,
    templates::{self, NewTemplate},
};
use loco_domain::ability::Action;
use serde_json::{Value, json};

use super::{CreateError, Document, basename, create_attachments, normalize_attachment_fields};
use crate::{
    api::{
        self,
        params::{FormParams, UploadedFile},
        serialize::time,
    },
    auth::RequireAuth,
    errors::AppError,
    locale,
    session::Session,
    signing::actions::{redirect_to, request_origin},
    state::AppState,
    webhooks::{self, EventRecord},
};

/// `ActiveStorage::Attachment#signed_key` lifetime.
const SIGNED_KEY_HOURS: i64 = 6;

/// `params[:files].presence || params[:file]`.
fn uploaded_files(form: &FormParams) -> Vec<&UploadedFile> {
    let files: Vec<&UploadedFile> = form
        .files
        .iter()
        .filter(|(name, _)| name == "files[]" || name == "files")
        .map(|(_, file)| file)
        .collect();
    if files.is_empty() {
        form.file("file").into_iter().collect()
    } else {
        files
    }
}

/// `POST /templates_upload`: a new template from the uploaded documents, then the
/// template builder.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    form: FormParams,
) -> Result<Response, AppError> {
    let params = &form.params;
    if params.str("url").is_some() {
        return Ok(api::unprocessable(
            "Uploading documents from a URL is not supported yet",
        ));
    }
    let files: Vec<&UploadedFile> = form
        .files
        .iter()
        .filter(|(name, _)| name == "files[]" || name == "files")
        .map(|(_, file)| file)
        .collect();
    let attachments = uploaded_files(&form);
    if let Some(message) = super::unsupported(&attachments) {
        return Ok(api::unprocessable(message));
    }

    let locale = locale::app_locale(&state, Some(&auth), None);
    let failed = |state: &AppState, err: &dyn std::fmt::Display| {
        tracing::error!(%err, "template upload failed");
        session.flash_alert(state.views.t(&locale, "unable_to_update_file", &[]));
        redirect_to(state, &headers, "/")
    };

    // `save_template!`: the folder is found or created before the name is read.
    let folder_id = templates::find_or_create_folder_by_name(
        &state.db,
        auth.account.id,
        auth.user.id,
        &params.str("folder_name").unwrap_or_default(),
    )
    .await?;
    let Some(first) = files.first() else {
        return Ok(failed(&state, &"no files[] given"));
    };
    let first_party = json!([{
        "name": state.views.t(&locale, "first_party", &[]),
        "uuid": uuid::Uuid::new_v4().to_string(),
    }]);
    let template = templates::insert(
        &state.db,
        &NewTemplate {
            name: basename(&super::rack_filename(&first.filename)),
            slug: loco_rails_compat::base58(14),
            schema: JsonText(json!([])),
            fields: JsonText(json!([])),
            submitters: JsonText(first_party.clone()),
            author_id: auth.user.id,
            account_id: auth.account.id,
            source: "native".into(),
            folder_id,
            external_id: None,
            preferences: JsonText(json!({})),
            shared_link: false,
            variables_schema: None,
        },
    )
    .await?;

    let password = params.str("password");
    let mut documents =
        match create_attachments(&state, template.id, &attachments, password.as_deref(), true)
            .await
        {
            Ok(documents) => documents,
            Err(CreateError::PdfEncrypted) => {
                return Ok(prompt_password(
                    &params.str("form_id").unwrap_or_default(),
                ));
            }
            Err(CreateError::Failed(err)) => return Ok(failed(&state, &err)),
        };

    let mut schema: Vec<Value> = documents
        .iter()
        .map(|doc| {
            json!({
                "attachment_uuid": doc.attachment.uuid,
                "name": basename(&doc.blob.filename),
            })
        })
        .collect();
    let first_party_uuid = first_party[0]["uuid"].as_str().unwrap_or_default().to_owned();
    let fields = normalize_attachment_fields(&mut documents, &first_party_uuid);
    if !fields.is_empty() {
        for item in &mut schema {
            item["pending_fields"] = true.into();
        }
    }
    templates::update_schema_and_fields(
        &state.db,
        template.id,
        &JsonText(Value::Array(schema)),
        &JsonText(Value::Array(fields)),
    )
    .await?;

    webhooks::enqueue(
        &state,
        "template.created",
        &[EventRecord {
            id: template.id,
            account_id: template.account_id,
        }],
    )
    .await;
    crate::search::reindex_later(&state, "Template", &[template.id]).await;

    Ok(redirect_to(
        &state,
        &headers,
        &format!("/templates/{}/edit", template.id),
    ))
}

/// `turbo_stream.append(params[:form_id], html: tag.prompt_password)`.
fn prompt_password(form_id: &str) -> Response {
    let body = format!(
        "<turbo-stream action=\"append\" target=\"{}\"><template><prompt-password></prompt-password></template></turbo-stream>",
        loco_views::assets::escape_attr(form_id)
    );
    let mut response = (StatusCode::OK, Body::from(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/vnd.turbo-stream.html; charset=utf-8"),
    );
    response
}

/// `POST /templates/:template_id/documents`: documents added in the template builder.
pub async fn create_documents(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    form: FormParams,
) -> Result<Response, AppError> {
    let template = match template_id.parse::<i64>() {
        Ok(id) => templates::find(&state.db, id).await?,
        Err(_) => None,
    };
    let Some(template) = template else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    // `load_and_authorize_resource :template` (`:show`) and `authorize!(:update, …)`.
    if !api::templates::can(&state, &auth, &template, Action::Read).await?
        || !api::templates::can(&state, &auth, &template, Action::Update).await?
    {
        session.flash_alert("You are not authorized to access this page.");
        return Ok(redirect_to(&state, &headers, "/"));
    }

    let locale = locale::app_locale(&state, Some(&auth), None);
    let attachments = uploaded_files(&form);
    let has_blobs = form.params.get("blobs").is_some_and(|b| match b {
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => false,
    });
    if !has_blobs && attachments.is_empty() {
        return Ok(api::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({ "error": state.views.t(&locale, "file_is_missing", &[]) }),
        ));
    }
    if let Some(message) = super::unsupported(&attachments) {
        return Ok(api::unprocessable(message));
    }

    let documents = match create_attachments(&state, template.id, &attachments, None, true).await {
        Ok(documents) => documents,
        Err(CreateError::PdfEncrypted) => {
            return Ok(api::render(
                StatusCode::UNPROCESSABLE_ENTITY,
                &json!({ "error": "PDF encrypted", "status": "pdf_encrypted" }),
            ));
        }
        Err(CreateError::Failed(err)) => return Err(AppError::Internal(err)),
    };

    let origin = request_origin(&state, &headers);
    let schema: Vec<Value> = documents
        .iter()
        .map(|doc| {
            json!({
                "attachment_uuid": doc.attachment.uuid,
                "name": basename(&doc.blob.filename),
            })
        })
        .collect();
    let documents: Vec<Value> = documents
        .iter()
        .map(|doc| document_json(&state, &origin, doc))
        .collect();
    // The template's fields are unchanged here, so Rails answers `nil` for both.
    Ok(api::render(
        StatusCode::OK,
        &json!({
            "schema": schema,
            "fields": null,
            "submitters": null,
            "documents": documents,
        }),
    ))
}

fn attachment_attributes(attachment: &loco_storage::Attachment) -> serde_json::Map<String, Value> {
    let mut json = serde_json::Map::new();
    json.insert("id".into(), attachment.id.into());
    json.insert("blob_id".into(), attachment.blob_id.into());
    json.insert("created_at".into(), time(attachment.created_at));
    json.insert("name".into(), attachment.name.clone().into());
    json.insert("record_id".into(), attachment.record_id.into());
    json.insert("record_type".into(), attachment.record_type.clone().into());
    json.insert("uuid".into(), attachment.uuid.clone().into());
    json
}

/// `documents.as_json(methods: %i[metadata signed_key], include: { preview_images:
/// { methods: %i[url metadata filename] } })`.
fn document_json(state: &AppState, origin: &str, document: &Document) -> Value {
    let mut json = attachment_attributes(&document.attachment);
    json.insert("metadata".into(), document.metadata.clone());
    json.insert(
        "signed_key".into(),
        state
            .secrets
            .signed_id_verifier
            .generate(
                &json!([document.attachment.id, document.attachment.uuid]),
                Some("attachment"),
                Some(Utc::now() + Duration::hours(SIGNED_KEY_HOURS)),
            )
            .into(),
    );
    let previews: Vec<Value> = document
        .previews
        .iter()
        .map(|(attachment, blob)| {
            let mut preview = attachment_attributes(attachment);
            preview.insert(
                "url".into(),
                crate::disk::blob_url(state, origin, blob).into(),
            );
            preview.insert("metadata".into(), blob.metadata_json());
            preview.insert("filename".into(), blob.filename.clone().into());
            Value::Object(preview)
        })
        .collect();
    json.insert("preview_images".into(), Value::Array(previews));
    Value::Object(json)
}
