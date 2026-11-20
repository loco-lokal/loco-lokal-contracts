//! `TemplateDocumentsModifyController#create` (`POST /templates/:template_id/documents_modify`)
//! and `Templates::ModifyDocuments`: the template builder's page editor. Each entry of the
//! layout lists the pages of one schema document; changed documents are built again from
//! the pages of the template's PDFs and images (reordered, moved, turned, redacted), and
//! the field areas follow their pages.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use image::DynamicImage;
use loco_db::{JsonText, templates};
use loco_pdf::{
    image_page::Number,
    modify::{PageOps, Redaction, Run},
};
use loco_storage::{Attachment, Blob};
use serde_json::{Map, Value, json};

use super::{Document, PDF_CONTENT_TYPE, basename, images, previews};
use crate::{
    api::{self, params::Params},
    auth::RequireAuth,
    errors::AppError,
    locale,
    session::Session,
    signing::actions::request_origin,
    state::AppState,
};

type Size = (i64, i64);

/// `ModifyDocuments::A4_SIZE` and `LETTER_SIZE`, in points.
const A4_SIZE: Size = (595, 842);
const LETTER_SIZE: Size = (612, 792);
const PAGE_SIZE_TOLERANCE: f64 = 6.0;
const ROTATIONS: [i32; 4] = [0, 90, 180, 270];

/// `Templates::ModifyDocuments::InvalidLayout`.
#[derive(Debug)]
struct InvalidLayout;

/// A page of the layout (`pages: [:attachment_uuid, :page, :rotate, { redact:,
/// replaced_page: }]`), with the JSON values as permitted.
#[derive(Clone, Debug)]
struct PageRef {
    attachment_uuid: Value,
    page: Value,
    rotate: Value,
    /// `redact: [%i[x y w h color]]`: the permitted hashes, `None` when absent.
    redact: Option<Vec<Map<String, Value>>>,
    replaced_page: Option<(Value, Value)>,
}

impl PageRef {
    fn uuid(&self) -> Option<&str> {
        self.attachment_uuid.as_str()
    }

    /// `ref['rotate'].to_i % 360`.
    fn rotate(&self) -> i64 {
        ruby_to_i(&self.rotate).rem_euclid(360)
    }

    fn redactions(&self) -> Vec<Redaction> {
        self.redact
            .iter()
            .flatten()
            .map(|rect| {
                let num = |key: &str| rect.get(key).and_then(Value::as_f64).unwrap_or(0.0);
                Redaction {
                    x: num("x"),
                    y: num("y"),
                    w: num("w"),
                    h: num("h"),
                    white: rect.get("color").and_then(Value::as_str) == Some("white"),
                }
            })
            .collect()
    }

    fn has_redact(&self) -> bool {
        self.redact.as_ref().is_some_and(|r| !r.is_empty())
    }
}

/// An entry of the layout: a schema document and its pages.
#[derive(Clone, Debug)]
struct Entry {
    attachment_uuid: Value,
    pages: Vec<PageRef>,
}

/// `Integer#to_i`, `Float#to_i`, `String#to_i` and `nil.to_i`.
#[allow(clippy::cast_possible_truncation, reason = "Float#to_i truncates")]
fn ruby_to_i(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .unwrap_or_default(),
        Value::String(s) => api::params::to_i(s),
        _ => 0,
    }
}

fn permitted_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

fn scalar(object: &Map<String, Value>, key: &str) -> Value {
    object
        .get(key)
        .filter(|v| permitted_scalar(v))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Why the layout can't be read the way Rails reads it (`NoMethodError`, `TypeError`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct MalformedLayout(String);

/// `params.require(:documents).map { |item| item.permit(…).to_h }`. `Ok(None)` is
/// `ActionController::ParameterMissing`.
fn documents_layout(params: &Params) -> Result<Option<Vec<Entry>>, MalformedLayout> {
    let documents = match params.get("documents") {
        None | Some(Value::Null | Value::Bool(false)) => return Ok(None),
        Some(Value::Array(items)) if items.is_empty() => return Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Value::Object(o)) if o.is_empty() => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(other) => {
            return Err(MalformedLayout(format!(
                "undefined method 'map' for {other}"
            )));
        }
    };
    documents
        .iter()
        .map(|item| {
            let Value::Object(item) = item else {
                return Err(MalformedLayout(format!(
                    "undefined method 'permit' for {item}"
                )));
            };
            let pages = match item.get("pages") {
                Some(Value::Array(pages)) => pages
                    .iter()
                    .filter_map(Value::as_object)
                    .map(page_ref)
                    .collect::<Result<_, _>>()?,
                Some(Value::Object(pages)) if !pages.is_empty() => {
                    return Err(MalformedLayout(
                        "no implicit conversion of String into Integer".into(),
                    ));
                }
                _ => Vec::new(),
            };
            Ok(Entry {
                attachment_uuid: scalar(item, "attachment_uuid"),
                pages,
            })
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

fn page_ref(page: &Map<String, Value>) -> Result<PageRef, MalformedLayout> {
    let redact = match page.get("redact") {
        Some(Value::Array(rects)) => Some(
            rects
                .iter()
                .filter_map(Value::as_object)
                .map(|rect| {
                    ["x", "y", "w", "h", "color"]
                        .into_iter()
                        .filter(|key| rect.contains_key(*key))
                        .map(|key| (key.to_owned(), scalar(rect, key)))
                        .collect()
                })
                .collect(),
        ),
        _ => None,
    };
    let replaced_page = match page.get("replaced_page") {
        Some(Value::Object(replaced)) => Some((
            scalar(replaced, "attachment_uuid"),
            scalar(replaced, "page"),
        )),
        Some(Value::Array(items)) if items.iter().any(Value::is_object) => {
            return Err(MalformedLayout(
                "no implicit conversion of String into Integer".into(),
            ));
        }
        _ => None,
    };
    Ok(PageRef {
        attachment_uuid: scalar(page, "attachment_uuid"),
        page: scalar(page, "page"),
        rotate: scalar(page, "rotate"),
        redact,
        replaced_page,
    })
}

/// A template document with its blob.
#[derive(Clone, Debug)]
struct Source {
    attachment: Attachment,
    blob: Blob,
}

impl Source {
    fn is_image(&self) -> bool {
        self.blob
            .content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/"))
    }

    fn content_type(&self) -> &str {
        self.blob.content_type.as_deref().unwrap_or_default()
    }

    /// `page_count(attachment)`.
    fn page_count(&self) -> i64 {
        if self.content_type() == PDF_CONTENT_TYPE {
            self.blob
                .metadata_json()
                .pointer("/pdf/number_of_pages")
                .map_or(0, ruby_to_i)
        } else {
            1
        }
    }
}

fn index_get<'a>(index: &'a HashMap<String, Source>, uuid: &Value) -> Option<&'a Source> {
    uuid.as_str().and_then(|uuid| index.get(uuid))
}

fn schema_uuid(item: &Value) -> Value {
    item.get("attachment_uuid").cloned().unwrap_or(Value::Null)
}

/// Ruby truthiness of a schema item's `dynamic`.
fn dynamic(item: &Value) -> bool {
    !matches!(
        item.get("dynamic"),
        None | Some(Value::Null | Value::Bool(false))
    )
}

/// `validate_layout!`.
fn validate_layout(
    schema: &[Value],
    layout: &[Entry],
    index: &HashMap<String, Source>,
) -> Result<(), InvalidLayout> {
    if layout.is_empty() || layout.iter().all(|e| e.pages.is_empty()) {
        return Err(InvalidLayout);
    }
    let dynamic_uuids: Vec<Value> = schema
        .iter()
        .filter(|i| dynamic(i))
        .map(schema_uuid)
        .collect();
    let layout_uuids: Vec<&Value> = layout.iter().map(|e| &e.attachment_uuid).collect();
    let mut unique = layout_uuids.clone();
    unique.dedup_by(|a, b| a == b);
    if (0..layout_uuids.len()).any(|i| layout_uuids[..i].contains(&layout_uuids[i])) {
        return Err(InvalidLayout);
    }
    let missing = schema
        .iter()
        .map(schema_uuid)
        .filter(|uuid| !dynamic_uuids.contains(uuid))
        .any(|uuid| !layout_uuids.contains(&&uuid));
    if missing
        || layout_uuids.iter().any(|uuid| dynamic_uuids.contains(uuid))
        || layout_uuids
            .iter()
            .any(|uuid| index_get(index, uuid).is_none())
    {
        return Err(InvalidLayout);
    }

    let refs: Vec<&PageRef> = layout.iter().flat_map(|e| &e.pages).collect();
    for page in &refs {
        validate_ref(page, index)?;
    }
    let keys: Vec<(&Value, &Value)> = refs.iter().map(|r| (&r.attachment_uuid, &r.page)).collect();
    if (0..keys.len()).any(|i| keys[..i].contains(&keys[i])) {
        return Err(InvalidLayout);
    }
    Ok(())
}

/// `validate_ref!` and `validate_redact!`.
fn validate_ref(page: &PageRef, index: &HashMap<String, Source>) -> Result<(), InvalidLayout> {
    let source = index_get(index, &page.attachment_uuid).ok_or(InvalidLayout)?;
    let number = page
        .page
        .as_i64()
        .filter(|_| page.page.is_i64() || page.page.is_u64());
    if !number.is_some_and(|n| n >= 0 && n < source.page_count()) {
        return Err(InvalidLayout);
    }
    let rotate_ok = match &page.rotate {
        Value::Null => true,
        Value::Number(n) => n.as_f64().is_some_and(|f| {
            ROTATIONS
                .iter()
                .any(|&r| (f64::from(r) - f).abs() < f64::EPSILON)
        }),
        _ => false,
    };
    if !rotate_ok {
        return Err(InvalidLayout);
    }
    for rect in page.redact.iter().flatten() {
        let valid = ["x", "y", "w", "h"].iter().all(|key| {
            rect.get(*key)
                .and_then(Value::as_f64)
                .is_some_and(|v| (-1.0..=2.0).contains(&v))
        });
        if !valid {
            return Err(InvalidLayout);
        }
    }
    Ok(())
}

/// `unchanged_entry?`: the document's own pages, in order, not turned or redacted.
fn unchanged_entry(entry: &Entry, index: &HashMap<String, Source>) -> bool {
    let Some(source) = index_get(index, &entry.attachment_uuid) else {
        return false;
    };
    i64::try_from(entry.pages.len()).ok() == Some(source.page_count())
        && entry.pages.iter().enumerate().all(|(i, page)| {
            page.attachment_uuid == entry.attachment_uuid
                && page.page.as_i64() == i64::try_from(i).ok()
                && ruby_to_i(&page.rotate) == 0
                && !page.has_redact()
        })
}

/// Where a page of the old documents ends up: `[attachment_uuid, page, rotate]`.
#[derive(Clone, Debug)]
struct Target {
    uuid: String,
    page: usize,
    rotate: i64,
}

type Mapping = Vec<((Value, Value), Target)>;

fn add_page_mapping(mapping: &mut Mapping, page: &PageRef, target: &Target) {
    let mut set = |key: (Value, Value)| match mapping.iter_mut().find(|(k, _)| *k == key) {
        Some((_, existing)) => *existing = target.clone(),
        None => mapping.push((key, target.clone())),
    };
    set((page.attachment_uuid.clone(), page.page.clone()));
    if let Some((uuid, number)) = &page.replaced_page {
        set((uuid.clone(), number.clone()));
    }
}

/// The image options of a page in a run (`[redact.presence, rotate]` for images).
#[derive(Clone, Debug, PartialEq)]
struct ImageOps {
    redact: Option<Vec<Redaction>>,
    rotate: i64,
}

/// `build_page_runs`: consecutive pages of the same document (and, for images, the same
/// options), with their 1-based page numbers.
fn build_page_runs(
    pages: &[PageRef],
    index: &HashMap<String, Source>,
) -> Vec<(String, String, i32, Option<ImageOps>)> {
    let mut runs: Vec<(String, Vec<i64>, Option<ImageOps>)> = Vec::new();
    for page in pages {
        let uuid = page.uuid().unwrap_or_default().to_owned();
        let image_ops = index_get(index, &page.attachment_uuid)
            .is_some_and(Source::is_image)
            .then(|| ImageOps {
                redact: page.has_redact().then(|| page.redactions()),
                rotate: page.rotate(),
            });
        let number = page.page.as_i64().unwrap_or_default();
        match runs.last_mut() {
            Some((last_uuid, numbers, ops)) if *last_uuid == uuid && *ops == image_ops => {
                numbers.push(number);
            }
            _ => runs.push((uuid, vec![number], image_ops)),
        }
    }
    runs.into_iter()
        .map(|(uuid, numbers, ops)| {
            let pages = numbers
                .iter()
                .map(|n| (n + 1).to_string())
                .collect::<Vec<_>>()
                .join(",");
            (uuid, pages, i32::try_from(numbers.len()).unwrap_or(0), ops)
        })
        .collect()
}

/// `standard_page_size`: US Letter or A4, either way round, within 6 points.
fn standard_page_size(width: f64, height: f64) -> Option<Size> {
    [LETTER_SIZE, A4_SIZE].into_iter().find(|&(w, h)| {
        [(w, h), (h, w)].iter().any(|&(bw, bh)| {
            #[allow(clippy::cast_precision_loss, reason = "page sizes in points")]
            let (bw, bh) = (bw as f64, bh as f64);
            (width - bw).abs() <= PAGE_SIZE_TOLERANCE && (height - bh).abs() <= PAGE_SIZE_TOLERANCE
        })
    })
}

/// `aspect_page_size`: the paper size whose shape the image has.
fn aspect_page_size(width: u32, height: u32) -> Option<Size> {
    let (short, long) = (i64::from(width.min(height)), i64::from(width.max(height)));
    [LETTER_SIZE, A4_SIZE]
        .into_iter()
        .find(|&(page_short, page_long)| {
            (short * page_long - long * page_short).abs() <= page_short
        })
}

/// `default_page_size(account)`.
fn default_page_size(timezone: &str) -> Size {
    if crate::l10n::uses_us_letter(timezone) {
        LETTER_SIZE
    } else {
        A4_SIZE
    }
}

/// The key of a built source (`[uuid, image_ops]`, with the sizes for images).
#[derive(Clone, Debug, PartialEq)]
struct SourceKey {
    uuid: String,
    image_ops: Option<ImageOps>,
    sizes: Option<(Option<Size>, Size)>,
}

struct Builder<'a> {
    state: &'a AppState,
    template_id: i64,
    timezone: String,
    index: &'a HashMap<String, Source>,
    /// `sources`: opened PDFs and the PDFs built from images, by key.
    sources: Vec<(SourceKey, Vec<u8>)>,
}

impl Builder<'_> {
    async fn download(&self, source: &Source) -> Result<Bytes, AppError> {
        Ok(self.state.storage.download(&source.blob).await?)
    }

    async fn pdf_source(&mut self, uuid: &str) -> Result<usize, AppError> {
        let key = SourceKey {
            uuid: uuid.to_owned(),
            image_ops: None,
            sizes: None,
        };
        if let Some(i) = self.sources.iter().position(|(k, _)| *k == key) {
            return Ok(i);
        }
        let source = self
            .index
            .get(uuid)
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
        let data = self.download(source).await?.to_vec();
        self.sources.push((key, data));
        Ok(self.sources.len() - 1)
    }

    /// `entry_pdf_page_size`: the size of the entry's last PDF page, when it is US Letter
    /// or A4.
    async fn entry_pdf_page_size(&mut self, pages: &[PageRef]) -> Result<Option<Size>, AppError> {
        let Some(page) = pages
            .iter()
            .rev()
            .find(|p| index_get(self.index, &p.attachment_uuid).is_some_and(|s| !s.is_image()))
        else {
            return Ok(None);
        };
        let source = self.pdf_source(page.uuid().unwrap_or_default()).await?;
        let data = self.sources[source].1.clone();
        let number = i32::try_from(page.page.as_i64().unwrap_or_default()).unwrap_or_default();
        let size: Option<(f32, f32)> =
            blocking_pdf(move |pdfium| loco_pdf::modify::page_size(pdfium, &data, number)).await?;
        let size = size
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("undefined method '-' for nil")))?;
        let (mut width, mut height) = (f64::from(size.0), f64::from(size.1));
        if page.rotate() % 180 != 0 {
            std::mem::swap(&mut width, &mut height);
        }
        Ok(standard_page_size(width, height)
            .map(|(w, h)| if width > height { (h, w) } else { (w, h) }))
    }

    /// `open_or_build_pdf` for an image: `build_pdf_data_from_image`.
    async fn image_source(
        &mut self,
        uuid: &str,
        image_ops: &ImageOps,
        pdf_size: Option<Size>,
        default_size: Size,
    ) -> Result<usize, AppError> {
        let key = SourceKey {
            uuid: uuid.to_owned(),
            image_ops: Some(image_ops.clone()),
            sizes: Some((pdf_size, default_size)),
        };
        if let Some(i) = self.sources.iter().position(|(k, _)| *k == key) {
            return Ok(i);
        }
        let source = self
            .index
            .get(uuid)
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
        let previews = Attachment::with_blobs_for_records(
            &self.state.db,
            "ActiveStorage::Attachment",
            &[source.attachment.id],
            super::PREVIEW_IMAGES,
        )
        .await?;
        let (_, preview) = previews.first().ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("undefined method 'download' for nil"))
        })?;
        let png = self.state.storage.download(preview).await?;
        let ops = image_ops.clone();
        let data = tokio::task::spawn_blocking(move || {
            build_pdf_data_from_image(&png, &ops, pdf_size, default_size)
        })
        .await??;
        self.sources.push((key, data));
        Ok(self.sources.len() - 1)
    }

    /// `build_document`: the entry's pages in a new PDF, saved as a new document.
    async fn build_document(
        &mut self,
        schema_item_uuid: &str,
        pages: &[PageRef],
    ) -> Result<Document, AppError> {
        let with_images = pages
            .iter()
            .any(|p| index_get(self.index, &p.attachment_uuid).is_some_and(Source::is_image));
        let (pdf_size, default_size) = if with_images {
            (
                self.entry_pdf_page_size(pages).await?,
                default_page_size(&self.timezone),
            )
        } else {
            (None, A4_SIZE)
        };

        let mut runs = Vec::new();
        for (uuid, numbers, length, image_ops) in build_page_runs(pages, self.index) {
            let source = match &image_ops {
                Some(ops) => {
                    self.image_source(&uuid, ops, pdf_size, default_size)
                        .await?
                }
                None => self.pdf_source(&uuid).await?,
            };
            runs.push(Run {
                source,
                pages: numbers,
                length,
            });
        }
        let ops: Vec<Option<PageOps>> = pages
            .iter()
            .map(|page| {
                (!index_get(self.index, &page.attachment_uuid).is_some_and(Source::is_image)).then(
                    || PageOps {
                        rotate: i32::try_from(page.rotate()).unwrap_or(0),
                        redact: page.redactions(),
                    },
                )
            })
            .collect();
        let sources: Vec<Vec<u8>> = self.sources.iter().map(|(_, d)| d.clone()).collect();
        let data = blocking_pdf(move |pdfium| {
            let sources: Vec<&[u8]> = sources.iter().map(Vec::as_slice).collect();
            loco_pdf::modify::build(pdfium, &sources, &runs, &ops, &images::redacted_jpeg)
        })
        .await?;

        let old = self
            .index
            .get(schema_item_uuid)
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
        let filename = format!("{}.pdf", basename(&old.blob.filename));
        Ok(super::create_pdf_document(self.state, self.template_id, data, &filename).await?)
    }

    /// `build_image_document`: an entry of one image page, turned or redacted as a new
    /// image document (else the image itself).
    async fn build_image_document(&self, page: &PageRef) -> Result<Option<Document>, AppError> {
        let source = index_get(self.index, &page.attachment_uuid)
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
        if !page.has_redact() && page.rotate() == 0 {
            return Ok(None);
        }
        let data = self.download(source).await?;
        let content_type = source.content_type().to_owned();
        let (redact, rotate) = (page.redactions(), page.rotate());
        let jpeg = content_type == "image/jpeg";
        let (data, extension, content_type) = tokio::task::spawn_blocking(move || {
            let mut image = previews::decode(&data, &content_type, true)?;
            if !redact.is_empty() {
                images::draw_redaction(&mut image, &redact);
            }
            let image = images::rotate(image, rotate);
            images::write_document(&image, jpeg)
        })
        .await?
        .map_err(anyhow::Error::from)?;
        let filename = format!("{}{extension}", basename(&source.blob.filename));
        Ok(Some(
            super::create_image_document(
                self.state,
                self.template_id,
                Bytes::from(data),
                &filename,
                content_type,
            )
            .await?,
        ))
    }
}

async fn blocking_pdf<R: Send + 'static>(
    f: impl FnOnce(&loco_pdf::Pdfium) -> Result<R, String> + Send + 'static,
) -> Result<R, AppError> {
    tokio::task::spawn_blocking(move || loco_pdf::with_shared(f))
        .await?
        .map_err(anyhow::Error::from)?
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Pdfium::PdfiumError: {e}")))
}

/// `build_pdf_data_from_image`: the image's first preview, flattened, redacted and
/// turned, as a page of US Letter or A4 (`build_image_page_pdf`).
fn build_pdf_data_from_image(
    png: &[u8],
    ops: &ImageOps,
    pdf_size: Option<Size>,
    default_size: Size,
) -> Result<Vec<u8>, AppError> {
    let image = previews::decode(png, "image/png", false).map_err(anyhow::Error::from)?;
    let mut image = images::flatten_white(match image {
        DynamicImage::ImageLuma8(_) | DynamicImage::ImageLumaA8(_) => {
            DynamicImage::ImageRgba8(image.to_rgba8())
        }
        other => other,
    });
    if let Some(redact) = &ops.redact {
        images::draw_redaction(&mut image, redact);
    }
    let image = images::rotate(image, ops.rotate);
    let png = images::palette_png(&image).map_err(anyhow::Error::from)?;
    build_image_page_pdf(&image, &png, pdf_size, default_size)
}

/// `build_image_page_pdf`.
fn build_image_page_pdf(
    image: &DynamicImage,
    png: &[u8],
    pdf_size: Option<Size>,
    default_size: Size,
) -> Result<Vec<u8>, AppError> {
    let (iw, ih) = (image.width(), image.height());
    let landscape = iw > ih;
    let pdf_size = pdf_size.filter(|&(w, h)| (w > h) == landscape);
    let aspect_size = if pdf_size.is_none() {
        aspect_page_size(iw, ih)
    } else {
        None
    };
    let (page_width, page_height) = pdf_size.unwrap_or_else(|| {
        let (w, h) = aspect_size.unwrap_or(default_size);
        if landscape { (h, w) } else { (w, h) }
    });
    #[allow(clippy::cast_precision_loss, reason = "page sizes in points")]
    let (pw, ph) = (page_width as f64, page_height as f64);
    let (fw, fh) = (f64::from(iw), f64::from(ih));
    let scale = (pw / fw).min(ph / fh);
    let pdf = if pdf_size.is_none() && aspect_size.is_none() && !images::scanned_page(image) {
        loco_pdf::image_page::build(png, fw * scale, fh * scale, None)
    } else {
        let (image_width, image_height) = (fw * scale, fh * scale);
        loco_pdf::image_page::build_sized(
            png,
            [Number::Int(page_width), Number::Int(page_height)],
            Some([
                (pw - image_width) / 2.0,
                (ph - image_height) / 2.0,
                image_width,
                image_height,
            ]),
        )
    };
    Ok(pdf.map_err(|e| anyhow::anyhow!("Templates::BuildImagePagePdf::InvalidPng: {e}"))?)
}

/// Ruby's `Integer - Float` and friends, for area coordinates that may be either.
fn sub(a: &Value, b: &Value) -> Result<Value, AppError> {
    let err = || AppError::Internal(anyhow::anyhow!("undefined method '-' for nil"));
    if let (Some(x), Some(y)) = (
        a.as_i64().filter(|_| a.is_i64()),
        b.as_i64().filter(|_| b.is_i64()),
    ) {
        return Ok(json!(x - y));
    }
    let (x, y) = (a.as_f64().ok_or_else(err)?, b.as_f64().ok_or_else(err)?);
    Ok(json!(x - y))
}

/// `rotate_area(area, rotate)`.
fn rotate_area(area: &mut Map<String, Value>, rotate: i64) -> Result<(), AppError> {
    let get = |key: &str| area.get(key).cloned().unwrap_or(Value::Null);
    let (x, y, w, h) = (get("x"), get("y"), get("w"), get("h"));
    let one = json!(1);
    match rotate {
        90 => {
            let nx = sub(&sub(&one, &y)?, &h)?;
            area.insert("x".into(), nx);
            area.insert("y".into(), x);
            area.insert("w".into(), h);
            area.insert("h".into(), w);
        }
        180 => {
            let nx = sub(&sub(&one, &x)?, &w)?;
            let ny = sub(&sub(&one, &y)?, &h)?;
            area.insert("x".into(), nx);
            area.insert("y".into(), ny);
        }
        270 => {
            let ny = sub(&sub(&one, &x)?, &w)?;
            area.insert("x".into(), y);
            area.insert("y".into(), ny);
            area.insert("w".into(), h);
            area.insert("h".into(), w);
        }
        _ => {}
    }
    Ok(())
}

/// `remap_fields`: areas move with their pages; fields left without areas are removed.
fn remap_fields(
    fields: &mut Vec<Value>,
    non_dynamic_uuids: &[Value],
    mapping: &Mapping,
) -> Result<Vec<Value>, AppError> {
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for mut field in std::mem::take(fields) {
        let areas = field
            .get("areas")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .cloned();
        if let Some(areas) = areas {
            let mut new_areas = Vec::new();
            for area in areas {
                let uuid = area.get("attachment_uuid").cloned().unwrap_or(Value::Null);
                if !non_dynamic_uuids.contains(&uuid) {
                    new_areas.push(area);
                    continue;
                }
                let page = area.get("page").cloned().unwrap_or(Value::Null);
                let Some((_, target)) = mapping.iter().find(|((u, p), _)| *u == uuid && *p == page)
                else {
                    continue;
                };
                let Value::Object(mut area) = area else {
                    continue;
                };
                area.insert("attachment_uuid".into(), target.uuid.clone().into());
                area.insert("page".into(), target.page.into());
                rotate_area(&mut area, target.rotate)?;
                new_areas.push(Value::Object(area));
            }
            let empty = new_areas.is_empty();
            field["areas"] = Value::Array(new_areas);
            if empty {
                removed.push(field.get("uuid").cloned().unwrap_or(Value::Null));
                continue;
            }
        }
        kept.push(field);
    }
    *fields = kept;
    Ok(removed)
}

/// `remove_conditions(items, removed_field_uuids)`.
fn remove_conditions(items: &mut [Value], removed: &[Value]) {
    if removed.is_empty() {
        return;
    }
    for item in items {
        let Some(conditions) = item
            .get_mut("conditions")
            .and_then(Value::as_array_mut)
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        conditions.retain(|c| !removed.contains(c.get("field_uuid").unwrap_or(&Value::Null)));
    }
}

/// `Templates::ModifyDocuments.call(template, documents_layout)`: the new schema and
/// fields, saved.
async fn modify_documents(
    state: &AppState,
    template: &templates::Template,
    timezone: &str,
    layout: &[Entry],
) -> Result<Result<(Vec<Value>, Vec<Value>), InvalidLayout>, AppError> {
    let uuids: Vec<String> = layout
        .iter()
        .flat_map(|e| {
            std::iter::once(&e.attachment_uuid).chain(e.pages.iter().map(|p| &p.attachment_uuid))
        })
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let index: HashMap<String, Source> =
        Attachment::with_blobs_for_records(&state.db, "Template", &[template.id], super::DOCUMENTS)
            .await?
            .into_iter()
            .filter(|(a, _)| uuids.contains(&a.uuid))
            .map(|(attachment, blob)| (attachment.uuid.clone(), Source { attachment, blob }))
            .collect();

    let schema = template.schema.as_array().cloned().unwrap_or_default();
    if let Err(invalid) = validate_layout(&schema, layout, &index) {
        return Ok(Err(invalid));
    }

    let mut builder = Builder {
        state,
        template_id: template.id,
        timezone: timezone.to_owned(),
        index: &index,
        sources: Vec::new(),
    };
    let mut mapping: Mapping = Vec::new();
    let mut new_schema = Vec::new();
    for entry in layout {
        let schema_item = schema
            .iter()
            .find(|item| schema_uuid(item) == entry.attachment_uuid)
            .cloned()
            .unwrap_or_else(|| {
                let name = index_get(&index, &entry.attachment_uuid)
                    .map(|s| basename(&s.blob.filename))
                    .unwrap_or_default();
                json!({ "attachment_uuid": entry.attachment_uuid, "name": name })
            });
        if entry.pages.is_empty() {
            continue;
        }
        if unchanged_entry(entry, &index) {
            for (i, page) in entry.pages.iter().enumerate() {
                let target = Target {
                    uuid: page.uuid().unwrap_or_default().to_owned(),
                    page: i,
                    rotate: 0,
                };
                add_page_mapping(&mut mapping, page, &target);
            }
            new_schema.push(schema_item);
            continue;
        }
        let standalone_image = entry.pages.len() == 1
            && index_get(&index, &entry.pages[0].attachment_uuid).is_some_and(Source::is_image);
        let document_uuid = if standalone_image {
            match builder.build_image_document(&entry.pages[0]).await? {
                Some(document) => document.attachment.uuid,
                None => entry.pages[0].uuid().unwrap_or_default().to_owned(),
            }
        } else {
            let uuid = schema_uuid(&schema_item);
            builder
                .build_document(uuid.as_str().unwrap_or_default(), &entry.pages)
                .await?
                .attachment
                .uuid
        };
        for (i, page) in entry.pages.iter().enumerate() {
            let target = Target {
                uuid: document_uuid.clone(),
                page: i,
                rotate: page.rotate(),
            };
            add_page_mapping(&mut mapping, page, &target);
        }
        let mut item = schema_item;
        if let Value::Object(map) = &mut item {
            map.shift_remove("google_drive_file_id");
            map.insert("attachment_uuid".into(), document_uuid.into());
        }
        new_schema.push(item);
    }

    for (i, item) in schema.iter().enumerate() {
        if dynamic(item) {
            new_schema.insert(i.min(new_schema.len()), item.clone());
        }
    }

    let non_dynamic_uuids: Vec<Value> = schema
        .iter()
        .filter(|i| !dynamic(i))
        .map(schema_uuid)
        .collect();
    let mut fields = template.fields.as_array().cloned().unwrap_or_default();
    let removed = remap_fields(&mut fields, &non_dynamic_uuids, &mapping)?;
    remove_conditions(&mut fields, &removed);
    remove_conditions(&mut new_schema, &removed);

    if Value::Array(new_schema.clone()) != *template.schema
        || Value::Array(fields.clone()) != *template.fields
    {
        templates::update_schema_and_fields(
            &state.db,
            template.id,
            &JsonText(Value::Array(new_schema.clone())),
            &JsonText(Value::Array(fields.clone())),
        )
        .await?;
    }
    Ok(Ok((new_schema, fields)))
}

/// `schema_documents.reload.preload(:blob, preview_images_attachments: :blob)`.
pub(super) async fn schema_documents(
    state: &AppState,
    template_id: i64,
    schema: &[Value],
) -> Result<Vec<Document>, AppError> {
    let uuids: Vec<&str> = schema
        .iter()
        .filter_map(|i| i.get("attachment_uuid").and_then(Value::as_str))
        .collect();
    let mut attachments = Vec::new();
    for name in [super::DOCUMENTS] {
        attachments.extend(
            Attachment::with_blobs_for_records(&state.db, "Template", &[template_id], name).await?,
        );
    }
    attachments.retain(|(a, _)| uuids.contains(&a.uuid.as_str()));
    attachments.sort_by_key(|(a, _)| a.id);
    let previews = Attachment::with_blobs_for_records(
        &state.db,
        "ActiveStorage::Attachment",
        &attachments.iter().map(|(a, _)| a.id).collect::<Vec<_>>(),
        super::PREVIEW_IMAGES,
    )
    .await?;
    Ok(attachments
        .into_iter()
        .map(|(attachment, blob)| {
            let previews = previews
                .iter()
                .filter(|(p, _)| p.record_id == attachment.id)
                .cloned()
                .collect();
            Document {
                metadata: blob.metadata_json(),
                attachment,
                blob,
                previews,
            }
        })
        .collect())
}

/// `POST /templates/:template_id/documents_modify`.
pub async fn create(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    session: Session,
    headers: HeaderMap,
    Path(template_id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let template =
        match super::uploads::editable_template(&state, &auth, &session, &headers, &template_id)
            .await?
        {
            Ok(template) => template,
            Err(response) => return Ok(response),
        };
    let layout = match documents_layout(&params) {
        Ok(Some(layout)) => layout,
        Ok(None) => return Err(AppError::Status(StatusCode::BAD_REQUEST)),
        Err(err) => return Err(AppError::Internal(err.into())),
    };

    let Ok((schema, fields)) =
        modify_documents(&state, &template, &auth.account.timezone, &layout).await?
    else {
        let locale = locale::app_locale(&state, Some(&auth), None);
        return Ok(api::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({ "error": state.views.t(&locale, "unable_to_save", &[]) }),
        ));
    };

    let origin = request_origin(&state, &headers);
    let documents: Vec<Value> = schema_documents(&state, template.id, &schema)
        .await?
        .iter()
        .map(|doc| super::uploads::document_json(&state, &origin, doc))
        .collect();
    Ok(api::render(
        StatusCode::OK,
        &json!({
            "schema": schema,
            "fields": fields,
            "submitters": template.submitters.0,
            "documents": documents,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(uuid: &str, number: i64) -> PageRef {
        PageRef {
            attachment_uuid: json!(uuid),
            page: json!(number),
            rotate: Value::Null,
            redact: None,
            replaced_page: None,
        }
    }

    #[test]
    fn rotates_areas_like_rails() {
        let mut area = json!({ "x": 0.1, "y": 0.2, "w": 0.3, "h": 0.4, "page": 0 });
        rotate_area(area.as_object_mut().unwrap(), 90).unwrap();
        assert_eq!(area["x"].as_f64().unwrap(), 1.0 - 0.2 - 0.4);
        assert_eq!(area["y"], json!(0.1));
        assert_eq!(
            (area["w"].clone(), area["h"].clone()),
            (json!(0.4), json!(0.3))
        );

        let mut area = json!({ "x": 0, "y": 0, "w": 1, "h": 1 });
        rotate_area(area.as_object_mut().unwrap(), 180).unwrap();
        assert_eq!(area, json!({ "x": 0, "y": 0, "w": 1, "h": 1 }));
    }

    #[test]
    fn groups_consecutive_pages_into_runs() {
        let index = HashMap::new();
        let runs = build_page_runs(
            &[page("a", 0), page("a", 2), page("b", 0), page("a", 1)],
            &index,
        );
        let runs: Vec<(String, String, i32)> =
            runs.into_iter().map(|(u, p, l, _)| (u, p, l)).collect();
        assert_eq!(
            runs,
            [
                ("a".into(), "1,3".into(), 2),
                ("b".into(), "1".into(), 1),
                ("a".into(), "2".into(), 1)
            ]
        );
    }

    #[test]
    fn finds_paper_sizes() {
        assert_eq!(standard_page_size(595.28, 841.89), Some(A4_SIZE));
        assert_eq!(standard_page_size(792.0, 612.0), Some(LETTER_SIZE));
        assert_eq!(standard_page_size(500.0, 500.0), None);
        assert_eq!(aspect_page_size(1400, 1812), Some(LETTER_SIZE));
        assert_eq!(aspect_page_size(1400, 1981), Some(A4_SIZE));
        assert_eq!(aspect_page_size(1400, 466), None);
    }
}
