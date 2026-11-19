//! `TemplatesDetectFieldsController#create` (`POST /templates/:template_id/detect_fields`):
//! detects fields on the template's documents with the ONNX model (`loco_detect`) and
//! streams them as server-sent events, one `{attachment_uuid, page, fields}` per page,
//! then `{completed: true}`.
//!
//! As with `ActionController::Live`, the response is always a 200 event stream once the
//! template is authorized: an error ends the stream where it happens, without the
//! `completed` event (an empty body when nothing was written yet).
//!
//! Not supported yet (422 before anything is streamed): models other than the v2 one
//! (with an `orig_target_sizes` input), and image documents whose first preview is not
//! an 8-bit color PNG.

use std::convert::Infallible;

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use loco_detect::{
    Model, PageNodes,
    image::{PngError, Rgb},
};
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{DOCUMENTS, PREVIEW_IMAGES};
use crate::{
    api::{
        self,
        params::{Params, to_i},
    },
    auth::MaybeAuth,
    errors::AppError,
    session::Session,
    state::AppState,
    template_preview::readable_template,
};

/// A document to detect fields on.
enum Source {
    Pdf { uuid: String, blob: Blob },
    Image { uuid: String, preview: Preview },
}

/// The first preview of an image document.
enum Preview {
    /// Missing or not downloadable: Rails fails before detecting.
    Missing,
    /// Not an image `ImageUtils.load_vips` reads: Rails fails when detecting page 0.
    Broken,
    Image(Rgb),
}

/// `params[:page].presence&.to_i`; `Err` where Ruby raises (`true.to_i`).
fn page_number(params: &Params) -> Result<Option<i64>, ()> {
    match params.get("page") {
        None | Some(Value::Null | Value::Bool(false)) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_i64().or_else(|| {
            #[allow(clippy::cast_possible_truncation, reason = "Float#to_i")]
            n.as_f64().map(|f| f as i64)
        })),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(to_i(s))),
        Some(Value::Array(a)) if a.is_empty() => Ok(None),
        Some(Value::Object(o)) if o.is_empty() => Ok(None),
        Some(_) => Err(()),
    }
}

/// `params[:attachment_uuid]` as the uuids `where(uuid:)` matches, when present.
fn attachment_uuids(params: &Params) -> Option<Vec<String>> {
    let as_uuid = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    match params.get("attachment_uuid")? {
        Value::Array(items) if !items.is_empty() => {
            Some(items.iter().filter_map(as_uuid).collect())
        }
        Value::String(s) if !s.trim().is_empty() => Some(vec![s.clone()]),
        value @ Value::Number(_) => Some(as_uuid(value).into_iter().collect()),
        _ => None,
    }
}

fn event(data: &Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", api::rails_json(data)))
}

fn stream_response(rx: mpsc::Receiver<Bytes>) -> Response {
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, Infallible>(chunk), rx))
    });
    let mut response = (StatusCode::OK, Body::from_stream(stream)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// An event stream that ends right away, as Rails' after an error.
fn empty_stream(err: &dyn std::fmt::Display) -> Response {
    tracing::error!(%err, "field detection failed");
    let (_, rx) = mpsc::channel(1);
    stream_response(rx)
}

/// The first preview of an image document (`0.png` or `0.jpg`), decoded as
/// `ImageUtils.load_vips` would, or `Err` with the 422 message for previews this port
/// can't read.
async fn image_preview(state: &AppState, document: &Attachment) -> Result<Preview, String> {
    let previews = Attachment::with_blobs_for_records(
        &state.db,
        "ActiveStorage::Attachment",
        &[document.id],
        PREVIEW_IMAGES,
    )
    .await
    .map_err(|e| e.to_string())?;
    let Some((_, blob)) = previews
        .into_iter()
        .find(|(_, b)| b.filename == "0.png" || b.filename == "0.jpg")
    else {
        return Ok(Preview::Missing);
    };
    let Ok(data) = state.storage.download(&blob).await else {
        return Ok(Preview::Missing);
    };
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(
            "Detecting fields in image documents with JPEG previews is not supported yet".into(),
        );
    }
    match loco_detect::image::load_png(&data) {
        Ok(image) => Ok(Preview::Image(image)),
        Err(PngError::Invalid) => Ok(Preview::Broken),
        Err(PngError::Unsupported) => Err(
            "Detecting fields in image documents with grayscale or 16-bit previews is not supported yet"
                .into(),
        ),
    }
}

/// `POST /templates/:template_id/detect_fields` with `attachment_uuid` (one or many)
/// and `page`.
pub async fn create(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    // Devise's `throw :warden` can't leave the `ActionController::Live` thread, so Rails
    // answers signed-out requests with its 500 page instead of the sign-in redirect.
    let Some(auth) = auth else {
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    };
    let template = match readable_template(&state, &auth, &session, &headers, &template_id).await? {
        Ok(template) => template,
        Err(mut response) => {
            // The redirect, with the headers of a `Live` response.
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            return Ok(response);
        }
    };
    let Ok(page) = page_number(&params) else {
        return Ok(empty_stream(&"undefined method 'to_i' for page"));
    };

    // `@template.documents.where(uuid:)`, else `@template.schema_documents` (the
    // template's attachments with a schema uuid, which are its documents).
    let all =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template.id], DOCUMENTS).await;
    let documents: Vec<(Attachment, Blob)> = match (all, attachment_uuids(&params)) {
        (Ok(all), Some(uuids)) => all
            .into_iter()
            .filter(|(a, _)| uuids.contains(&a.uuid))
            .collect(),
        (Ok(all), None) => {
            let uuids: Vec<&str> = template
                .schema
                .0
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("attachment_uuid").and_then(Value::as_str))
                .collect();
            all.into_iter()
                .filter(|(a, _)| uuids.contains(&a.uuid.as_str()))
                .collect()
        }
        (Err(err), _) => return Ok(empty_stream(&err)),
    };

    let app_root = state.config.app_root.clone();
    let model = match tokio::task::spawn_blocking(move || Model::shared(&app_root)).await {
        Ok(Ok(model)) => model,
        Ok(Err(err)) => return Ok(empty_stream(&err)),
        Err(err) => return Ok(empty_stream(&err)),
    };
    if !model.v2 {
        return Ok(api::unprocessable(
            "Field detection with this model is not supported yet",
        ));
    }

    let mut sources = Vec::new();
    for (attachment, blob) in documents {
        let image = blob
            .content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image"));
        if image {
            match image_preview(&state, &attachment).await {
                Ok(preview) => sources.push(Source::Image {
                    uuid: attachment.uuid,
                    preview,
                }),
                Err(message) => return Ok(api::unprocessable(message)),
            }
        } else {
            sources.push(Source::Pdf {
                uuid: attachment.uuid,
                blob,
            });
        }
    }

    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(detect(state, model, sources, page, tx));
    Ok(stream_response(rx))
}

/// Streams the fields of each document, then `completed`; stops at the first error.
async fn detect(
    state: AppState,
    model: &'static Model,
    sources: Vec<Source>,
    page: Option<i64>,
    tx: mpsc::Sender<Bytes>,
) {
    for source in sources {
        let sent = match source {
            Source::Image { uuid, preview } => detect_image(model, uuid, preview, page, &tx).await,
            Source::Pdf { uuid, blob } => detect_pdf(&state, model, uuid, &blob, page, &tx).await,
        };
        if let Err(err) = sent {
            tracing::error!(error = format!("{err:#}"), "field detection failed");
            return;
        }
    }
    let _ = tx.send(event(&json!({ "completed": true }))).await;
}

async fn detect_image(
    model: &'static Model,
    uuid: String,
    preview: Preview,
    page: Option<i64>,
    tx: &mpsc::Sender<Bytes>,
) -> anyhow::Result<()> {
    if matches!(preview, Preview::Missing) {
        anyhow::bail!("no preview image");
    }
    if page.is_some_and(|p| p != 0) {
        return Ok(());
    }
    let Preview::Image(image) = preview else {
        anyhow::bail!("unreadable preview image");
    };
    let fields = {
        let uuid = uuid.clone();
        tokio::task::spawn_blocking(move || loco_detect::detect_image(model, &image, Some(&uuid)))
            .await??
    };
    send(tx, &uuid, 0, fields).await
}

async fn send(
    tx: &mpsc::Sender<Bytes>,
    uuid: &str,
    page: i64,
    fields: Vec<Value>,
) -> anyhow::Result<()> {
    tx.send(event(&json!({
        "attachment_uuid": uuid,
        "page": page,
        "fields": fields,
    })))
    .await
    .map_err(|_| anyhow::anyhow!("the client went away"))
}

async fn detect_pdf(
    state: &AppState,
    model: &'static Model,
    uuid: String,
    blob: &Blob,
    page: Option<i64>,
    tx: &mpsc::Sender<Bytes>,
) -> anyhow::Result<()> {
    let data = state.storage.download(blob).await?;
    let pages: Vec<i64> = if let Some(page) = page {
        vec![page]
    } else {
        let data = data.clone();
        let count = tokio::task::spawn_blocking(move || {
            loco_pdf::with_shared(|pdfium| loco_pdf::page_count(pdfium, &data))
        })
        .await???;
        (0..i64::try_from(count)?).collect()
    };
    let size = loco_detect::render_size(model);
    let mut nodes = PageNodes::default();
    for index in pages {
        let data = data.clone();
        let attachment_uuid = uuid.clone();
        let (fields, next) = tokio::task::spawn_blocking(move || {
            let page = loco_pdf::with_shared(|pdfium| {
                loco_pdf::pages::detect_page(pdfium, &data, index, size)
            })?
            .map_err(anyhow::Error::msg)?;
            let fields = loco_detect::detect_pdf_page(
                model,
                &mut nodes,
                &page,
                index,
                Some(&attachment_uuid),
            )?;
            anyhow::Ok((fields, nodes))
        })
        .await??;
        nodes = next;
        send(tx, &uuid, index, fields).await?;
    }
    Ok(())
}
