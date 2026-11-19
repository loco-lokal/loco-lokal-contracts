//! Document pages outside the signing form: `PreviewDocumentPageController` (the lazy
//! `/preview/:signed_key/:page` images), `TemplatesPreviewController` (the read-only
//! template builder) and `TemplateDocumentsPageObjectsController` (the text and image
//! boxes of a document page).

use axum::body::Bytes;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use loco_db::templates::{self, Template};
use loco_domain::ability::Action;
use loco_storage::{Attachment, Blob};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    api::{params::to_i, serialize},
    auth::{Auth, RequireAuth},
    errors::AppError,
    locale,
    page::Page,
    session::Session,
    signing::actions::{redirect_to, request_origin},
    state::AppState,
};

/// `Templates::ProcessDocument::PREVIEW_FORMAT` quality (`PAGE_QUALITY`, default 35).
fn jpeg_quality() -> u32 {
    std::env::var("PAGE_QUALITY")
        .ok()
        .map_or(35, |q| u32::try_from(to_i(&q)).unwrap_or(0))
}

/// `head`/error content type: the MIME type of the path's format (`16.jpg`), else HTML.
fn format_content_type(page: &str) -> &'static str {
    match page
        .split_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        Some(ext) if ext == "jpg" || ext == "jpeg" => "image/jpeg",
        Some(ext) if ext == "png" => "image/png",
        _ => "text/html",
    }
}

fn head(status: StatusCode, content_type: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
    )
        .into_response()
}

/// The exceptions app's 500 page, served with the request format's content type.
fn server_error(state: &AppState, page: &str, err: &anyhow::Error) -> Response {
    tracing::error!(error = format!("{err:#}"), "preview failed");
    let html = std::fs::read_to_string(state.config.app_root.join("app/views/errors/500.html"))
        .unwrap_or_default();
    let content_type = format!("{}; charset=utf-8", format_content_type(page));
    let mut response = (StatusCode::INTERNAL_SERVER_ERROR, html).into_response();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// The attachment a signed key names: `[id, uuid]` (`signed_key`) or a uuid
/// (`signed_uuid`), as `ApplicationRecord.signed_id_verifier.verified(…, purpose:
/// :attachment)` returns them.
async fn signed_attachment(state: &AppState, key: &str) -> Result<Option<Attachment>, AppError> {
    let Ok(data) = state
        .secrets
        .signed_id_verifier
        .verify(key, Some("attachment"))
    else {
        return Ok(None);
    };
    let present = |v: &&Value| match v {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    };
    let uuids: Vec<String> = match &data {
        Value::Array(items) => {
            let items: Vec<&Value> = items.iter().filter(present).collect();
            if let [id, uuid] = items.as_slice() {
                let id = match id {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                };
                let attachment = match id {
                    Some(id) => Attachment::find(&state.db, id).await?,
                    None => None,
                };
                return Ok(attachment.filter(|a| uuid.as_str() == Some(a.uuid.as_str())));
            }
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        }
        Value::String(uuid) => vec![uuid.clone()],
        Value::Null | Value::Bool(false) => return Ok(None),
        other => vec![other.to_string()],
    };
    for uuid in uuids {
        if let Some(attachment) = Attachment::find_by_uuid(&state.db, &uuid).await? {
            return Ok(Some(attachment));
        }
    }
    Ok(None)
}

/// `GET /preview/:signed_key/:id`: redirects to the page's stored preview image, or
/// renders the page as a JPEG preview first (`generate_pdf_preview_from_io`).
pub async fn page_image(
    State(state): State<AppState>,
    Path((signed_key, page)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let not_found = || head(StatusCode::NOT_FOUND, format_content_type(&page));
    let Some(attachment) = signed_attachment(&state, &signed_key).await? else {
        return Ok(not_found());
    };
    let index = to_i(page.split_once('.').map_or(page.as_str(), |(id, _)| id));
    let origin = request_origin(&state, &headers);

    let names = [format!("{index}.png"), format!("{index}.jpg")];
    if let Some((_, blob)) = Attachment::with_blobs_for_records(
        &state.db,
        "ActiveStorage::Attachment",
        &[attachment.id],
        "preview_images",
    )
    .await?
    .into_iter()
    .find(|(_, blob)| names.contains(&blob.filename))
    {
        let blob = with_uuid(&state, blob).await?;
        return Ok(redirect(&crate::disk::blob_url(&state, &origin, &blob)));
    }

    match generate_preview(&state, &attachment, index).await {
        Ok(blob) => Ok(redirect(&crate::disk::blob_url(&state, &origin, &blob))),
        Err(err) => Ok(server_error(&state, &page, &err)),
    }
}

/// `redirect_to url, allow_other_host: true`.
fn redirect(url: &str) -> Response {
    let mut response = crate::request::found(url);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

async fn with_uuid(state: &AppState, mut blob: Blob) -> Result<Blob, AppError> {
    blob.ensure_uuid(&state.db).await?;
    Ok(blob)
}

/// Renders the page `MAX_WIDTH` wide, stores it as `"#{index}.jpg"` with Rails' blob
/// metadata, and attaches it to the document as a preview image.
async fn generate_preview(
    state: &AppState,
    attachment: &Attachment,
    index: i64,
) -> anyhow::Result<Blob> {
    let source = attachment.blob(&state.db).await?;
    let data = state.storage.download(&source).await?;
    let quality = jpeg_quality();
    let (jpeg, width, height) = tokio::task::spawn_blocking(move || {
        let bitmap =
            loco_pdf::with_shared(|pdfium| loco_pdf::pages::render_preview(pdfium, &data, index))?
                .map_err(anyhow::Error::msg)?;
        let (width, height) = (bitmap.width as usize, bitmap.height as usize);
        let jpeg = loco_pdf::jpeg::encode_rgba(&bitmap.rgba, width, height, quality);
        anyhow::Ok((jpeg, bitmap.width, bitmap.height))
    })
    .await??;

    let blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            Bytes::from(jpeg),
            &format!("{index}.jpg"),
            Some("image/jpeg"),
            json!({ "analyzed": true, "identified": true, "width": width, "height": height }),
        )
        .await?;
    Attachment::create(
        &state.db,
        "ActiveStorage::Attachment",
        attachment.id,
        "preview_images",
        &blob,
    )
    .await?;
    Ok(blob)
}

/// `load_and_authorize_resource :template` for a nested controller: the template of
/// `params[:template_id]`, which the user must be able to read. Unknown templates are
/// 404s; others redirect to `/` with `CanCan`'s message.
pub(crate) async fn readable_template(
    state: &AppState,
    auth: &Auth,
    session: &Session,
    headers: &HeaderMap,
    id: &str,
) -> Result<Result<Template, Response>, AppError> {
    let id = to_i(id);
    let Some(template) = templates::find(&state.db, id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let sharings = if template.account_id == auth.account.id {
        Vec::new()
    } else {
        templates::sharings(&state.db, template.id).await?
    };
    if auth
        .ability()
        .can_template(Action::Read, Some(template.account_id), &sharings)
    {
        Ok(Ok(template))
    } else {
        session.flash_alert("You are not authorized to access this page.");
        Ok(Err(redirect_to(state, headers, "/")))
    }
}

#[derive(Deserialize)]
pub struct PageObjectsParams {
    attachment_uuid: Option<String>,
    page: Option<String>,
}

/// `GET /templates/:template_id/documents_page_objects?attachment_uuid=&page=`.
pub async fn page_objects(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    Query(params): Query<PageObjectsParams>,
) -> Result<Response, AppError> {
    let template = match readable_template(&state, &auth, &session, &headers, &template_id).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    let Some((_, blob)) =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template.id], "documents")
            .await?
            .into_iter()
            .find(|(a, _)| params.attachment_uuid.as_deref() == Some(a.uuid.as_str()))
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let page = params.page.as_deref().map_or(0, to_i);
    let data = state.storage.download(&blob).await?;
    let objects = tokio::task::spawn_blocking(move || {
        loco_pdf::with_shared(|pdfium| loco_pdf::pages::page_objects(pdfium, &data, page))
    })
    .await??
    .map_err(anyhow::Error::msg)?;

    let coord = |c: loco_pdf::pages::Coord| match c {
        loco_pdf::pages::Coord::Int(i) => json!(i),
        loco_pdf::pages::Coord::Float(f) => json!(f),
    };
    let body = json!({
        "text_nodes": objects.text_nodes.iter().map(|n| json!({
            "text": n.text, "x": n.x, "y": n.y, "w": n.w, "h": n.h,
        })).collect::<Vec<_>>(),
        "image_nodes": objects.image_nodes.iter().map(|n| json!({
            "x": coord(n.x), "y": coord(n.y), "w": coord(n.w), "h": coord(n.h),
        })).collect::<Vec<_>>(),
    });
    let mut response = (StatusCode::OK, Body::from(crate::api::rails_json(&body))).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct ShowParams {
    lang: Option<String>,
}

/// `GET /templates/:template_id/preview`: the template builder, read-only, in the
/// `plain` layout.
pub async fn show(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    Query(params): Query<ShowParams>,
) -> Result<Response, AppError> {
    let template = match readable_template(&state, &auth, &session, &headers, &template_id).await? {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if user_agent.contains("Hotwire Native") {
        return Ok(crate::api::unprocessable(
            "The template preview in the Hotwire Native apps is not supported yet",
        ));
    }

    let origin = request_origin(&state, &headers);
    let data = builder_json(&state, &origin, &template).await?;
    let archived = template.archived_at.is_some();
    let locale = locale::app_locale(&state, Some(&auth), params.lang.as_deref());
    Page::new(&state, &session, locale)
        .auth(Some(&auth))
        .with(json!({
            "robots_noindex": true,
            // As ERB prints `true`/`false`; the send button also needs
            // `can?(:create, @template.submissions.new(account: current_account))`, which
            // holds for the user's own account.
            "with_sign_yourself_button": (!archived).to_string(),
            "with_send_button": (!archived).to_string(),
            "template_data": crate::api::rails_json(&data),
        }))
        .render("templates_preview/show.html")
}

/// `Templates.serialize_for_builder`: the template's builder fields, then its schema
/// documents with their metadata, signed key and preview images.
pub async fn builder_json(
    state: &AppState,
    origin: &str,
    template: &Template,
) -> Result<Value, AppError> {
    // `as_json(only: TEMPLATE_BUILDER_FIELDS)`, in the order of the model's attributes.
    let mut data = Map::new();
    data.insert("id".into(), template.id.into());
    data.insert("author_id".into(), template.author_id.into());
    data.insert("folder_id".into(), template.folder_id.into());
    data.insert(
        "external_id".into(),
        template
            .external_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    data.insert("name".into(), template.name.clone().into());
    data.insert("slug".into(), template.slug.clone().into());
    data.insert("schema".into(), template.schema.0.clone());
    data.insert("fields".into(), template.fields.0.clone());
    data.insert("submitters".into(), template.submitters.0.clone());
    data.insert(
        "variables_schema".into(),
        template
            .variables_schema
            .as_ref()
            .map_or(Value::Null, |v| v.0.clone()),
    );
    data.insert("preferences".into(), template.preferences.0.clone());
    data.insert("shared_link".into(), template.shared_link.into());
    data.insert("source".into(), template.source.clone().into());
    data.insert(
        "archived_at".into(),
        serialize::opt_time(template.archived_at),
    );
    data.insert("created_at".into(), serialize::time(template.created_at));
    data.insert("updated_at".into(), serialize::time(template.updated_at));

    let uuids: Vec<&str> = template
        .schema
        .0
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("attachment_uuid").and_then(Value::as_str))
        .collect();
    let documents: Vec<(Attachment, Blob)> =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template.id], "documents")
            .await?
            .into_iter()
            .filter(|(a, _)| uuids.contains(&a.uuid.as_str()))
            .collect();
    let ids: Vec<i64> = documents.iter().map(|(a, _)| a.id).collect();
    let previews = if ids.is_empty() {
        Vec::new()
    } else {
        Attachment::with_blobs_for_records(
            &state.db,
            "ActiveStorage::Attachment",
            &ids,
            "preview_images",
        )
        .await?
    };

    let expires_at = chrono::Utc::now() + chrono::Duration::hours(6);
    let mut list = Vec::new();
    for (attachment, blob) in &documents {
        let mut images = Vec::new();
        for (preview, preview_blob) in previews
            .iter()
            .filter(|(p, _)| p.record_id == attachment.id)
        {
            let preview_blob = if state.storage.is_disk(&preview_blob.service_name) {
                preview_blob.clone()
            } else {
                with_uuid(state, preview_blob.clone()).await?
            };
            images.push(json!({
                "id": preview.id,
                "url": crate::disk::blob_url(state, origin, &preview_blob),
                "metadata": preview_blob.metadata_json(),
                "filename": preview_blob.filename,
            }));
        }
        list.push(json!({
            "id": attachment.id,
            "uuid": attachment.uuid,
            "metadata": blob.metadata_json(),
            "signed_key": state.secrets.signed_id_verifier.generate(
                &json!([attachment.id, attachment.uuid]),
                Some("attachment"),
                Some(expires_at),
            ),
            "preview_images": images,
        }));
    }
    data.insert("documents".into(), Value::Array(list));
    Ok(Value::Object(data))
}
