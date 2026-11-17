//! `Submissions::GenerateResultAttachments` and `Submissions::EnsureResultGenerated`: the
//! completed documents of a submitter. Each document of the submission is loaded (from the
//! previous submitter's results, else the originals), the submitter's values are drawn
//! on it, and it is saved as an incremental update, signed when the submission is done
//! (or every time with the `multiple` e-signing preference).
//!
//! Two or more image documents are also merged into one more result PDF.
//!
//! Not supported yet (the job fails with a "not supported yet" error, so nothing differs
//! silently): dynamic documents, and form field appearances HexaPDF would fail to create
//! (`loco_pdf::edit::appearances`).
//!
//! With `PDF_FORMAT=pdf/a-3b`, the `:pdfa` task (level 3b) runs as each document is saved.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{NaiveDateTime, Utc};
use loco_db::{signing, submissions::Submitter};
use loco_domain::submit::{blank, filtered_conditions_schema, present};
use loco_pdf::edit::{
    EditError, Editor, ImageInfo,
    layout::{Align, Fitted, Fragment, InlineImage, Input, Layouter, Status, TextStyle, Valign},
    parse_color,
    pdfa::Pdfa,
    sign::SigningKey,
};
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};

use super::{Form, timestamp::TimestampHandler};
use crate::{errors::AppError, state::AppState};

const FONT_SIZE: f64 = 11.0;
pub(super) const FONT_NAME: &str = "GoNotoKurrent-Regular.ttf";
pub(super) const FONT_BOLD_NAME: &str = "GoNotoKurrent-Bold.ttf";
pub(super) const SIGN_REASON: &str = "Signed with DocuSeal.com";
pub(super) const UNSUPPORTED_IMAGE_TYPES: &[&str] = &["image/heic", "image/avif"];
const TEXT_LEFT_MARGIN: f64 = 1.0;
const TEXT_TOP_MARGIN: f64 = 1.0;
const A4_WIDTH: f64 = 595.0;
pub(super) const TESTING_FOOTER: &str = "Testing Document - NOT LEGALLY BINDING";
const DEFAULT_FONTS: &[&str] = &["Times", "Helvetica", "Courier"];
const CHECK_PNG: &[u8] = include_bytes!("../../../../../lib/pdf_icons/check.png");
const PAPERCLIP_PNG: &[u8] = include_bytes!("../../../../../lib/pdf_icons/paperclip.png");
/// `PdfIcons::WIDTH`, `PdfIcons::HEIGHT`.
const ICON_SIZE: f64 = 240.0;

const FLATTEN_RESULT_PDF_KEY: &str = "flatten_result_pdf";
pub(super) const WITH_SIGNATURE_ID: &str = "with_signature_id";
pub(super) const WITH_FILE_LINKS_KEY: &str = "with_file_links";
pub(super) const WITH_TIMESTAMP_SECONDS_KEY: &str = "with_timestamp_seconds";
const ROTATE_INCREMENTAL_PDF_KEY: &str = "rotate_incremental_pdf";
pub(super) const WITH_SUBMITTER_TIMEZONE_KEY: &str = "with_submitter_timezone";
const WITH_SIGNATURE_ID_REASON_KEY: &str = "with_signature_id_reason";
const WITH_SIGNATURE_ID_COMPLETED_AT_KEY: &str = "with_signature_id_completed_at";
const ESIGNING_PREFERENCE_KEY: &str = "esigning_preference";
pub(super) const TIMESTAMP_SERVER_URL_KEY: &str = "timestamp_server_url";
/// `Docuseal::AATL_CERT_NAME`.
const AATL_CERT_NAME: &str = "docuseal_aatl";
/// `Docuseal.pdf_format` for PDF/A-3b output.
pub(super) const PDF_A_3B: &str = "pdf/a-3b";

/// `EnsureResultGenerated::CHECK_COMPLETE_TIMEOUT`.
const CHECK_COMPLETE_TIMEOUT: Duration = Duration::from_secs(90);

pub(super) fn unsupported(what: &str) -> AppError {
    anyhow::anyhow!("{what} is not supported yet").into()
}

#[allow(clippy::needless_pass_by_value, reason = "used with map_err")]
pub(super) fn pdf_error(err: EditError) -> AppError {
    anyhow::anyhow!("result documents: {err}").into()
}

/// `Submissions::EnsureResultGenerated.call(submitter)`: the submitter's result documents,
/// generating them once (other workers wait for the one generating).
pub async fn ensure_result_generated(
    state: &AppState,
    submitter: &Submitter,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    if submitter.completed_at.is_none() {
        return Err(anyhow::anyhow!("submitter {} has not completed yet", submitter.id).into());
    }
    let key = format!("result_attachments:{}", submitter.id);
    let started = std::time::Instant::now();
    loop {
        let events = signing::lock_event_names(&state.db, &key).await?;
        if events.iter().any(|e| e == "complete") {
            return documents(state, submitter.id).await;
        }
        if events.last().is_some_and(|e| e == "start" || e == "retry") {
            return wait_for_complete_or_fail(state, submitter.id, &key).await;
        }
        let name = if events.is_empty() { "start" } else { "retry" };
        if !signing::create_lock_event(&state.db, &key, name).await? {
            // Another worker started at the same time.
            tokio::time::sleep(Duration::from_secs(2)).await;
            if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
                return Err(anyhow::anyhow!("timed out waiting for {key}").into());
            }
            continue;
        }
        return match Box::pin(generate(state, submitter)).await {
            Ok(documents) => {
                signing::create_lock_event(&state.db, &key, "complete").await?;
                Ok(documents)
            }
            Err(err) => {
                tracing::error!(error = ?err, submitter_id = submitter.id, "generating result documents failed");
                signing::create_lock_event(&state.db, &key, "fail").await?;
                Err(err)
            }
        };
    }
}

async fn wait_for_complete_or_fail(
    state: &AppState,
    submitter_id: i64,
    key: &str,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = signing::lock_event_names(&state.db, key).await?;
        if events
            .last()
            .is_some_and(|e| e == "complete" || e == "fail")
        {
            return documents(state, submitter_id).await;
        }
        if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
            return Err(anyhow::anyhow!("timed out waiting for {key}").into());
        }
    }
}

async fn documents(
    state: &AppState,
    submitter_id: i64,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    Ok(
        Attachment::with_blobs_for_records(&state.db, "Submitter", &[submitter_id], "documents")
            .await?,
    )
}

/// The account configs `generate_pdfs` reads.
struct Configs {
    with_signature_id: bool,
    flatten: bool,
    with_timestamp_seconds: bool,
    with_submitter_timezone: bool,
    with_file_links: bool,
    with_signature_id_reason: bool,
    with_signature_id_completed_at: bool,
}

impl Configs {
    async fn load(state: &AppState, account_id: i64) -> Result<Self, AppError> {
        let rows = signing::account_configs(
            &state.db,
            account_id,
            &[
                FLATTEN_RESULT_PDF_KEY,
                WITH_SIGNATURE_ID,
                WITH_FILE_LINKS_KEY,
                WITH_TIMESTAMP_SECONDS_KEY,
                ROTATE_INCREMENTAL_PDF_KEY,
                WITH_SUBMITTER_TIMEZONE_KEY,
                WITH_SIGNATURE_ID_REASON_KEY,
                WITH_SIGNATURE_ID_COMPLETED_AT_KEY,
            ],
        )
        .await?;
        let find = |key: &str| rows.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let is_true = |key: &str| find(key) == Some(&Value::Bool(true));
        let not_false = |key: &str| find(key) != Some(&Value::Bool(false));
        Ok(Self {
            with_signature_id: is_true(WITH_SIGNATURE_ID),
            flatten: not_false(FLATTEN_RESULT_PDF_KEY),
            with_timestamp_seconds: is_true(WITH_TIMESTAMP_SECONDS_KEY),
            with_submitter_timezone: is_true(WITH_SUBMITTER_TIMEZONE_KEY),
            with_file_links: is_true(WITH_FILE_LINKS_KEY),
            with_signature_id_reason: not_false(WITH_SIGNATURE_ID_REASON_KEY),
            with_signature_id_completed_at: is_true(WITH_SIGNATURE_ID_COMPLETED_AT_KEY),
        })
    }
}

/// A document being filled, by its schema `attachment_uuid`.
struct Pdf {
    uuid: String,
    editor: Editor,
    /// The schema document is an image (`original_documents.find { … }.image?`), even when
    /// the PDF is a previous party's result.
    is_image: bool,
}

/// `GenerateResultAttachments.call(submitter)`.
async fn generate(
    state: &AppState,
    submitter: &Submitter,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    let form = Form::for_submitter(state, submitter.clone()).await?;
    let configs = Configs::load(state, form.account.id).await?;
    let mut pdfs = build_pdfs_index(state, &form, configs.flatten).await?;

    if configs.with_signature_id || form.account.testing {
        add_document_id(state, &form, &configs, &mut pdfs)?;
    }
    fill_submitter_fields(state, &form, &configs, &mut pdfs).await?;

    let key = load_signing_key(state, form.account.id).await?;
    let timestamp = TimestampHandler::load(state, form.account.id)
        .await?
        .map(Arc::new);
    let reason = sign_reason(state, &form).await?;
    let signer = reason.zip(key).map(|(reason, key)| Signer {
        reason,
        key: Arc::new(key),
        timestamp,
    });

    let mut results = Vec::new();
    let mut image_pdfs: Vec<Vec<u8>> = Vec::new();
    for item in form.schema() {
        let Some(uuid) = item.get("attachment_uuid").and_then(Value::as_str) else {
            continue;
        };
        let Some(index) = pdfs.iter().position(|p| p.uuid == uuid) else {
            continue;
        };
        let mut pdf = pdfs.remove(index);
        if pdf.is_image {
            pdf.editor = pdf.editor.rewrite(&pdf_now()).map_err(pdf_error)?;
        }
        let bytes = save_pdf(state, pdf.editor, signer.as_ref()).await?;
        let name = item.get("name").map(ruby_to_s).unwrap_or_default();
        if pdf.is_image {
            image_pdfs.push(bytes.clone());
        }
        results.push(store_pdf(state, submitter, bytes, &pdf.uuid, &name).await?);
    }
    if image_pdfs.len() >= 2 {
        let sources: Vec<&[u8]> = image_pdfs.iter().map(Vec::as_slice).collect();
        let editor = Editor::merge(&sources, &pdf_now()).map_err(pdf_error)?;
        let bytes = save_pdf(state, editor, signer.as_ref()).await?;
        // `images_pdf_uuid(original_documents.select(&:image?))`: every image document of
        // the schema, also those hidden by conditions.
        let originals = super::pages::schema_documents(state, &form)
            .await?
            .unwrap_or_default();
        let mut uuids: Vec<&str> = originals
            .iter()
            .filter(|d| is_image(&d.blob))
            .map(|d| d.attachment.uuid.as_str())
            .collect();
        uuids.sort_unstable();
        let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, uuids.join(":").as_bytes());
        let name = form
            .submission
            .name
            .clone()
            .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
            .unwrap_or_default();
        results.push(store_pdf(state, submitter, bytes, &uuid.to_string(), &name).await?);
    }
    Ok(results)
}

fn pdf_now() -> String {
    Utc::now().format("D:%Y%m%d%H%M%S").to_string()
}

/// `Accounts.load_signing_pkcs(account)`: the certificates from `CERTS` when set, else the
/// account's (or the first account's) `esign_certs`: the custom certificate selected as
/// the default (a PKCS#12 file), or the generated certificates.
pub(super) async fn load_signing_key(
    state: &AppState,
    account_id: i64,
) -> Result<Option<SigningKey>, AppError> {
    if !state.config.certs.is_empty() {
        return default_pkcs(&state.config.certs);
    }
    let Some(encrypted) = signing::encrypted_config_with_fallback(
        &state.db,
        account_id,
        crate::state::ESIGN_CERTS_KEY,
    )
    .await?
    else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&state.secrets.encryptor.decrypt(&encrypted)?)?;
    let default_cert = value.get("custom").and_then(Value::as_array).and_then(|c| {
        c.iter()
            .find(|e| e.get("status").and_then(Value::as_str) == Some("default"))
    });
    if let Some(cert) = default_cert {
        if cert.get("name").and_then(Value::as_str) == Some(AATL_CERT_NAME) {
            return default_pkcs(&state.config.certs);
        }
        let data = cert.get("data").and_then(Value::as_str).unwrap_or_default();
        let der = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, data)
            .map_err(|e| anyhow::anyhow!("custom certificate: {e}"))?;
        let password = cert.get("password").map(ruby_to_s).unwrap_or_default();
        return SigningKey::from_pkcs12(&der, &password)
            .map(Some)
            .map_err(pdf_error);
    }
    load_pkcs(&value)
}

/// `Docuseal.default_pkcs`: none with `enabled: false`, else the `CERTS` certificates.
fn default_pkcs(certs: &serde_json::Map<String, Value>) -> Result<Option<SigningKey>, AppError> {
    if certs.get("enabled") == Some(&Value::Bool(false)) {
        return Ok(None);
    }
    if certs.is_empty() {
        // `GenerateCertificate.load_pkcs({})` raises in Rails.
        return Err(unsupported("Signing with the DocuSeal AATL certificate"));
    }
    load_pkcs(&Value::Object(certs.clone()))
}

/// `GenerateCertificate.load_pkcs(cert_data)`: PEM `cert`, `key`, `sub_ca` and `root_ca`.
fn load_pkcs(value: &Value) -> Result<Option<SigningKey>, AppError> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
    if text("key").is_empty() {
        if text("cert").is_empty() {
            return Ok(None);
        }
        // A certificate without its key: HexaPDF has nothing to sign with and the job
        // fails in Rails.
        return Err(unsupported(
            "Signing with a certificate without a private key",
        ));
    }
    let key = SigningKey::from_pem(
        text("cert"),
        text("key"),
        &[text("sub_ca"), text("root_ca")],
    )
    .map_err(pdf_error)?;
    Ok(Some(key))
}

/// `fetch_sign_reason(submitter)`.
async fn sign_reason(state: &AppState, form: &Form) -> Result<Option<String>, AppError> {
    let preference = signing::first_account_config(&state.db, ESIGNING_PREFERENCE_KEY).await?;
    if preference.as_ref().and_then(Value::as_str) == Some("multiple") {
        return Ok(Some(SIGN_REASON.to_owned()));
    }
    let submission = loco_db::submissions::find(&state.db, form.submission.id).await?;
    // `submitters.maximum(:completed_at)` is queried now: the submitters loaded with the
    // form may predate a later party's completion that the submission already records.
    let max = loco_db::submissions::submitters_for(&state.db, &[form.submission.id])
        .await?
        .iter()
        .filter_map(|s| s.completed_at)
        .max();
    if submission.is_some_and(|s| s.completed_at.is_some()) && form.submitter.completed_at == max {
        return Ok(Some(SIGN_REASON.to_owned()));
    }
    Ok(None)
}

/// `find_last_submitter(submission, submitter:)`.
fn find_last_submitter<'a>(form: &'a Form, submitter: &Submitter) -> Option<&'a Submitter> {
    form.submitters
        .iter()
        .filter(|s| s.id != submitter.id)
        .filter(|s| {
            s.completed_at
                .is_some_and(|at| submitter.completed_at.is_some_and(|own| at <= own))
        })
        .max_by_key(|s| s.completed_at)
}

/// `build_pdfs_index(submission, submitter:, flatten:)`.
async fn build_pdfs_index(
    state: &AppState,
    form: &Form,
    flatten: bool,
) -> Result<Vec<Pdf>, AppError> {
    let documents = match find_last_submitter(form, &form.submitter) {
        Some(last) => Box::pin(ensure_result_generated(state, last)).await?,
        None => Vec::new(),
    };
    let Some(schema_documents) = super::pages::schema_documents(state, form).await? else {
        return Err(unsupported("Generating results of dynamic documents"));
    };
    let documents = if documents.is_empty() {
        schema_documents
            .iter()
            .map(|d| (d.attachment.clone(), d.blob.clone()))
            .collect()
    } else {
        documents
    };
    let option_word = form.option_word(state);
    let schema = filtered_conditions_schema(
        form.schema(),
        form.fields(),
        &form.all_values(),
        None,
        &option_word,
    );
    let mut pdfs = Vec::new();
    for item in schema {
        let Some(uuid) = item.get("attachment_uuid").and_then(Value::as_str) else {
            continue;
        };
        let found = documents.iter().find(|(a, b)| {
            b.metadata_json()
                .get("original_uuid")
                .and_then(Value::as_str)
                .unwrap_or(&a.uuid)
                == uuid
        });
        let schema_document = schema_documents.iter().find(|d| d.attachment.uuid == uuid);
        let (blob, from_image) = match (found, schema_document) {
            (Some((_, blob)), _) => (blob, is_image(blob)),
            (None, Some(document)) => (&document.blob, is_image(&document.blob)),
            (None, None) => continue,
        };
        let mut editor = if from_image {
            let document = schema_document
                .ok_or_else(|| unsupported("An image document outside the schema"))?;
            let Some((_, preview)) = document.previews.first() else {
                continue;
            };
            let data = state.storage.download(preview).await?;
            let image = image::load_from_memory(&data)
                .map_err(|e| anyhow::anyhow!("preview image: {e}"))?;
            let meta = preview.metadata_json();
            let dimension = |key: &str, fallback: u32| {
                meta.get(key)
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::from(fallback))
            };
            Editor::from_image(
                &image,
                dimension("width", image.width()),
                dimension("height", image.height()),
            )
            .map_err(pdf_error)?
        } else {
            Editor::load(state.storage.download(blob).await?.to_vec()).map_err(pdf_error)?
        };
        editor.rotate_pages();
        if flatten {
            // `maybe_flatten_pdf`: appearances for the form's fields, then `flatten`.
            editor.create_form_appearances().map_err(pdf_error)?;
            editor.flatten_form();
        }
        for warning in std::mem::take(&mut editor.warnings) {
            tracing::warn!(document = uuid, "{warning}");
        }
        pdfs.push(Pdf {
            uuid: uuid.to_owned(),
            editor,
            is_image: schema_document.is_some_and(|d| is_image(&d.blob)),
        });
    }
    Ok(pdfs)
}

fn is_image(blob: &Blob) -> bool {
    blob.content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("image/"))
}

/// `pdf.trailer.info[:DocumentID]` and the footer on every page.
fn add_document_id(
    state: &AppState,
    form: &Form,
    configs: &Configs,
    pdfs: &mut [Pdf],
) -> Result<(), AppError> {
    use md5::{Digest as _, Md5};
    let document_id =
        Md5::digest(form.submission.slug.as_bytes())
            .iter()
            .fold(String::new(), |mut acc, b| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{b:02X}");
                acc
            });
    let text = if form.account.testing {
        if configs.with_signature_id {
            format!("{TESTING_FOOTER} | ID: {document_id}")
        } else {
            TESTING_FOOTER.to_owned()
        }
    } else {
        format!(
            "{}: {document_id}",
            state.views.t(&locale(form), "document_id", &[])
        )
    };
    for pdf in pdfs {
        let editor = &mut pdf.editor;
        if editor.info("DocumentID").is_some_and(|v| !v.is_empty()) {
            continue;
        }
        let (font, font_key) = editor.font(default_font(), "none").map_err(pdf_error)?;
        editor.set_info_text("DocumentID", &document_id);
        for page in editor.page_ids() {
            let [left, bottom, right, top] = editor.page_box(page);
            let (width, height) = (right - left, top - bottom);
            let font_size = ((width.min(height) / A4_WIDTH * 9.0).trunc()).max(4.0);
            let style = Arc::new(TextStyle {
                font: font.clone(),
                font_key: font_key.clone(),
                font_size,
                line_height: None,
                fill_color: None,
                white_underlay: true,
                underline: false,
                link: None,
            });
            let fitted = Layouter::new(Align::Left, Valign::Top).fit(
                vec![Input::Text(Fragment::new(&text, &style))],
                width,
                height,
                &style,
            );
            editor.draw_text(page, &fitted, 1.0, font_size * 1.37);
        }
    }
    Ok(())
}

/// `FONT_NAME`: GoNoto when the font files are installed, else Helvetica.
pub(super) fn default_font() -> &'static str {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *AVAILABLE.get_or_init(|| loco_pdf::edit::fonts::load(FONT_NAME, "none").is_ok()) {
        FONT_NAME
    } else {
        "Helvetica"
    }
}

fn locale(form: &Form) -> String {
    form.submitter
        .metadata
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or(&form.account.locale)
        .to_owned()
}

pub(super) fn ruby_to_s(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `Array.wrap(value)`.
pub(super) fn wrap(value: Option<&Value>) -> Vec<Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    }
}

/// The submitter's uploaded attachments with their blobs.
pub(super) struct Uploads {
    pub(super) items: Vec<(Attachment, Blob)>,
    pub(super) data: HashMap<String, Arc<Vec<u8>>>,
}

impl Uploads {
    pub(super) fn find(&self, uuid: &str) -> Option<&(Attachment, Blob)> {
        self.items.iter().find(|(a, _)| a.uuid == uuid)
    }

    async fn download(
        &mut self,
        state: &AppState,
        uuid: &str,
    ) -> Result<Option<Arc<Vec<u8>>>, AppError> {
        if let Some(data) = self.data.get(uuid) {
            return Ok(Some(data.clone()));
        }
        let Some((_, blob)) = self.find(uuid) else {
            return Ok(None);
        };
        let data = Arc::new(state.storage.download(blob).await?.to_vec());
        self.data.insert(uuid.to_owned(), data.clone());
        Ok(Some(data))
    }

    /// `ImageUtils.load_vips` with `GenerateResultAttachments`' rescue: `None` skips the area.
    pub(super) async fn image(
        &mut self,
        state: &AppState,
        uuid: &str,
    ) -> Result<Option<image::DynamicImage>, AppError> {
        let Some(data) = self.download(state, uuid).await? else {
            return Ok(None);
        };
        let (_, blob) = self.find(uuid).expect("downloaded");
        let content_type = blob.content_type.clone().unwrap_or_default();
        match crate::images::load(&data, &content_type) {
            Some(image) => Ok(Some(image)),
            None if !content_type.starts_with("image/") || blob.byte_size == 0 => Ok(None),
            None => Err(anyhow::anyhow!("unable to load image {uuid}").into()),
        }
    }
}

/// `image.resize([scale * 4, 1].select(&:positive?).min)`.
fn resize(image: &image::DynamicImage, scale: f64) -> image::DynamicImage {
    let factor = [scale * 4.0, 1.0]
        .into_iter()
        .filter(|v| *v > 0.0)
        .fold(f64::MAX, f64::min);
    if factor >= 1.0 {
        return image.clone();
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "image sizes"
    )]
    let (w, h) = (
        ((f64::from(image.width()) * factor).round() as u32).max(1),
        ((f64::from(image.height()) * factor).round() as u32).max(1),
    );
    image.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
}

/// `TextUtils::RTL_REGEXP`.
pub(super) fn is_rtl(text: &str) -> bool {
    loco_domain::bidi::is_rtl(text)
}

/// `TextUtils.maybe_rtl_reverse(text)`: right-to-left text in visual order. Characters
/// `TwitterCldr` has no data for fail the job, as they raise in Rails.
pub(super) fn maybe_rtl_reverse(text: &str) -> Result<String, AppError> {
    loco_domain::bidi::maybe_rtl_reverse(text).map_err(|e| anyhow::anyhow!("{e}").into())
}

/// `preferences.font_size` (`num.presence&.to_i`).
fn preferences_font_size(field: &Value) -> Option<f64> {
    let value = field.get("preferences")?.get("font_size")?;
    if blank(Some(value)) {
        return None;
    }
    #[allow(clippy::cast_precision_loss, reason = "font sizes")]
    match value {
        Value::Number(n) => n.as_f64().map(f64::trunc),
        Value::String(s) => {
            let digits: String = s
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            Some(digits.parse::<i64>().unwrap_or(0) as f64)
        }
        _ => Some(0.0),
    }
}

fn pref<'a>(field: &'a Value, key: &str) -> Option<&'a Value> {
    field.get("preferences").and_then(|p| p.get(key))
}

fn pref_str<'a>(field: &'a Value, key: &str) -> Option<&'a str> {
    pref(field, key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `field.dig('preferences', 'color')` as a fill color; unparseable colors are drawn black.
fn color(spec: Option<&str>) -> Option<[f64; 3]> {
    spec.map(|s| s.trim_start_matches('#'))
        .and_then(parse_color)
}

/// One field area's page and geometry.
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

struct Ctx<'a> {
    state: &'a AppState,
    form: &'a Form,
    configs: &'a Configs,
    uploads: Uploads,
    locale: String,
    file_links_expire_at: Option<chrono::DateTime<Utc>>,
}

/// `fill_submitter_fields`.
async fn fill_submitter_fields(
    state: &AppState,
    form: &Form,
    configs: &Configs,
    pdfs: &mut [Pdf],
) -> Result<(), AppError> {
    let items = Attachment::with_blobs_for_records(
        &state.db,
        "Submitter",
        &[form.submitter.id],
        "attachments",
    )
    .await?;
    let file_links_expire_at = if configs.with_file_links {
        crate::files::link_expires_at(state, form.account.id).await?
    } else {
        None
    };
    let mut ctx = Ctx {
        state,
        form,
        configs,
        uploads: Uploads {
            items,
            data: HashMap::new(),
        },
        locale: locale(form),
        file_links_expire_at,
    };
    let with_headings = find_last_submitter(form, &form.submitter).is_none();
    let submitter = &form.submitter;
    for field in form.fields() {
        let field_type = field
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let no_conditions = blank(field.get("conditions"));
        if !with_headings
            && (field_type == "heading" || (field_type == "strikethrough" && no_conditions))
        {
            continue;
        }
        if field.get("submitter_uuid").and_then(Value::as_str) != Some(submitter.uuid.as_str())
            && field_type != "heading"
            && (field_type != "strikethrough" || !no_conditions)
        {
            continue;
        }
        let areas = field
            .get("areas")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for area in &areas {
            let Some(uuid) = area.get("attachment_uuid").and_then(Value::as_str) else {
                continue;
            };
            let Some(pdf) = pdfs.iter_mut().find(|p| p.uuid == uuid) else {
                continue;
            };
            let Some(page_index) = area.get("page").and_then(Value::as_u64) else {
                continue;
            };
            let Some(&page) = pdf
                .editor
                .page_ids()
                .get(usize::try_from(page_index).unwrap_or(usize::MAX))
            else {
                continue;
            };
            fill_area(&mut ctx, &mut pdf.editor, page, field, area).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn fill_area(
    ctx: &mut Ctx<'_>,
    editor: &mut Editor,
    page: lopdf_id::ObjectId,
    field: &Value,
    area: &Value,
) -> Result<(), AppError> {
    let state = ctx.state;
    let form = ctx.form;
    let submitter = &form.submitter;
    editor.rotate_page_flat(page);
    editor.remove_field_links(page);

    let [left, bottom, right, top] = editor.page_box(page);
    let (width, height) = (right - left, top - bottom);
    let num = |key: &str| area.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let a = Area {
        x: num("x"),
        y: num("y"),
        w: num("w"),
        h: num("h"),
    };

    let preferences_font_size = preferences_font_size(field);
    let font_size =
        preferences_font_size.unwrap_or_else(|| (width.min(height) / A4_WIDTH * FONT_SIZE).trunc());
    let fill_color = color(pref_str(field, "color"));
    let bg_color = color(pref_str(field, "background"));

    let mut font_name = pref_str(field, "font").unwrap_or_default().to_owned();
    let mut font_variant = pref_str(field, "font_type").unwrap_or("none").to_owned();
    if !DEFAULT_FONTS.contains(&font_name.as_str()) {
        default_font().clone_into(&mut font_name);
    }
    if font_variant != "none" && font_name == default_font() {
        let mapped = match (default_font() == FONT_NAME, font_variant.as_str()) {
            (true, "bold") => Some(FONT_BOLD_NAME),
            (true, "italic" | "bold_italic") | (false, _) => Some("Helvetica"),
            _ => None,
        };
        if let Some(mapped) = mapped {
            mapped.clone_into(&mut font_name);
        }
        if !DEFAULT_FONTS.contains(&font_name.as_str()) {
            "none".clone_into(&mut font_variant);
        }
    }
    let (font, font_key) = editor.font(&font_name, &font_variant).map_err(pdf_error)?;
    let line_height_factor = match font_name.as_str() {
        "Times" | "Helvetica" => 1.5,
        "Courier" => 1.6,
        _ => 1.0,
    };
    let style = |size: f64, line_height: Option<f64>, fill: Option<[f64; 3]>| {
        Arc::new(TextStyle {
            font: font.clone(),
            font_key: font_key.clone(),
            font_size: size,
            line_height,
            fill_color: fill,
            white_underlay: false,
            underline: false,
            link: None,
        })
    };
    let layouter_style = style(font_size, None, None);

    let field_type_raw = field
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut value = submitter
        .values
        .get(
            field
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .cloned();
    if field_type_raw == "heading"
        || (field_type_raw == "strikethrough" && value.is_none() && blank(field.get("conditions")))
    {
        value = field.get("default_value").cloned();
    }
    let value_text = value.as_ref().map(ruby_to_s).unwrap_or_default();
    let text_align = match pref_str(field, "align") {
        Some("left") => Align::Left,
        Some("center") => Align::Center,
        Some("right") => Align::Right,
        Some("justify") => Align::Justify,
        _ if is_rtl(&value_text) => Align::Right,
        _ => Align::Left,
    };
    let valign_of = |field: &Value| match pref_str(field, "valign") {
        Some("top") => Valign::Top,
        Some("bottom") => Valign::Bottom,
        _ => Valign::Center,
    };
    let vertical_align = valign_of(field);
    let layouter = Layouter::new(text_align, vertical_align);
    let fit = |layouter: Layouter, items: Vec<Input>, w: f64, h: f64| {
        layouter.fit(items, w, h, &layouter_style)
    };

    if wrap(value.as_ref()).iter().all(|v| blank(Some(v))) {
        return Ok(());
    }
    if ctx.configs.flatten {
        editor.flatten_annotations(page, None);
    }

    let mut field_type = field_type_raw.to_owned();
    if (field_type == "image" || field_type == "stamp")
        && ctx.uploads.find(&value_text).is_some_and(|(_, blob)| {
            !is_image(blob)
                || UNSUPPORTED_IMAGE_TYPES
                    .contains(&blob.content_type.as_deref().unwrap_or_default())
        })
    {
        "file".clone_into(&mut field_type);
    }
    let mut with_signature_id = ctx.configs.with_signature_id;
    if field_type == "signature"
        && let Some(Value::Bool(own)) = pref(field, "with_signature_id")
    {
        with_signature_id = *own;
    }

    if let Some(bg) = bg_color {
        editor.overlay(page).fill_rect(
            bg,
            a.x * width,
            height - a.y * height - a.h * height,
            a.w * width,
            a.h * height,
        );
    }

    let text_fragment = |text: &str, size: f64| Fragment::new(text, &style(size, None, None));
    let has_reason_field = pref_str(field, "reason_field_uuid").is_some();

    match field_type.as_str() {
        "signature" if with_signature_id || has_reason_field => {
            let Some(image) = ctx.uploads.image(state, &value_text).await? else {
                return Ok(());
            };
            let attachment_created_at = ctx
                .uploads
                .find(&value_text)
                .map(|(a, _)| a.created_at)
                .unwrap_or_default();
            let reason_value = pref_str(field, "reason_field_uuid")
                .and_then(|uuid| submitter.values.get(uuid))
                .filter(|v| present(Some(v)))
                .map(ruby_to_s);
            let reason_string =
                reason_string(ctx, field, reason_value.as_deref(), attachment_created_at)?;
            let mut base_font_size = (font_size / 1.8).trunc();
            let (area_x, area_y, area_w, area_h) =
                (a.x * width, a.y * height, a.w * width, a.h * height);
            let (iw, ih) = (f64::from(image.width()), f64::from(image.height()));
            let id_text = format!("ID: {value_text}").to_uppercase();

            if area_h > 0.0 && area_w / area_h > 4.5 {
                let half_width = area_w / 2.0;
                let scale = (half_width / iw).min(area_h / ih);
                let (image_width, image_height) = (iw * scale, ih * scale);
                let image_x = area_x + (half_width - image_width) / 2.0;
                let image_y = height - area_y - image_height;
                let info = editor
                    .add_image(&resize(&image, scale), false)
                    .map_err(pdf_error)?;
                let name = editor.use_image(page, info.index);
                editor
                    .overlay(page)
                    .image(&name, image_x, image_y, image_width, image_height);

                let id_string = truncate_id(
                    layouter,
                    &layouter_style,
                    &id_text,
                    &style,
                    base_font_size,
                    half_width,
                );
                let string = format!("{id_string}\n{reason_string}");
                loop {
                    let result = fit(
                        layouter,
                        vec![Input::Text(text_fragment(&string, base_font_size))],
                        half_width,
                        area_h,
                    );
                    if result.status == Status::Success {
                        break;
                    }
                    base_font_size *= 0.9;
                    if base_font_size < 2.0 {
                        break;
                    }
                }
                let fitted = fit(
                    layouter,
                    vec![Input::Text(text_fragment(&string, base_font_size))],
                    half_width,
                    area_h,
                );
                editor.draw_text(
                    page,
                    &fitted,
                    area_x + half_width + TEXT_LEFT_MARGIN,
                    height - area_y,
                );
            } else {
                let reason_text = text_fragment(&reason_string, base_font_size);
                let (id_string, id_result) = truncate_id_result(
                    layouter,
                    &layouter_style,
                    &id_text,
                    &style,
                    base_font_size,
                    area_w,
                );
                let reason_result = fit(
                    layouter,
                    vec![Input::Text(reason_text.clone())],
                    area_w,
                    height,
                );
                let text_height = id_result.lines_height() + reason_result.lines_height();
                let mut image_height = area_h - text_height;
                if image_height < area_h / 2.0 {
                    image_height = area_h / 2.0;
                }
                let scale = (area_w / iw).min(image_height / ih);
                let fitted = fit(
                    layouter,
                    vec![Input::Text(text_fragment(&id_string, base_font_size))],
                    area_w,
                    base_font_size / 0.65,
                );
                editor.draw_text(
                    page,
                    &fitted,
                    area_x + TEXT_LEFT_MARGIN,
                    height - area_y - TEXT_TOP_MARGIN - image_height,
                );
                let fitted = fit(
                    layouter,
                    vec![Input::Text(reason_text)],
                    area_w,
                    reason_result.lines_height(),
                );
                editor.draw_text(
                    page,
                    &fitted,
                    area_x + TEXT_LEFT_MARGIN,
                    height - area_y - TEXT_TOP_MARGIN - id_result.lines_height() - image_height,
                );
                let info = editor
                    .add_image(&resize(&image, scale), false)
                    .map_err(pdf_error)?;
                let name = editor.use_image(page, info.index);
                editor.overlay(page).image(
                    &name,
                    area_x + area_w / 2.0 - iw * scale / 2.0,
                    height - area_y - ih * scale / 2.0 - image_height / 2.0,
                    iw * scale,
                    ih * scale,
                );
            }
        }
        "image" | "signature" | "initials" | "stamp" | "kba" => {
            let Some(image) = ctx.uploads.image(state, &value_text).await? else {
                return Ok(());
            };
            let (iw, ih) = (f64::from(image.width()), f64::from(image.height()));
            let scale = (a.w * width / iw).min(a.h * height / ih);
            let resized = resize(&image, scale);
            let jpeg = field_type == "image" && !resized.color().has_alpha();
            let info = editor.add_image(&resized, jpeg).map_err(pdf_error)?;
            let name = editor.use_image(page, info.index);
            editor.overlay(page).image(
                &name,
                a.x * width + a.w * width / 2.0 - iw * scale / 2.0,
                height - a.y * height - ih * scale / 2.0 - a.h * height / 2.0,
                iw * scale,
                ih * scale,
            );
        }
        "file" | "payment" => {
            let paperclip = editor
                .cached_image("paperclip", || {
                    image::load_from_memory(PAPERCLIP_PNG)
                        .map_err(|e| EditError::Image(e.to_string()))
                })
                .map_err(pdf_error)?;
            let uuids = wrap(value.as_ref());
            let text_style = style(font_size, None, None);
            let mut items = Vec::new();
            let mut linked = Vec::new();
            for uuid in &uuids {
                let Some((_, blob)) = ctx.uploads.find(&ruby_to_s(uuid)) else {
                    continue;
                };
                linked.push(blob.clone());
                items.push(Input::Inline(paperclip_box(paperclip, font_size)));
                items.push(Input::Text(Fragment::new(
                    &format!("{}\n", maybe_rtl_reverse(&blob.filename)?),
                    &text_style,
                )));
            }
            let lines = fit(layouter, items.clone(), a.w * width, height).lines;
            let heights: Vec<f64> = lines
                .iter()
                .map(loco_pdf::edit::layout::Line::height)
                .collect();
            let box_height: f64 = heights.iter().sum();
            let height_diff = (box_height - a.h * height).max(0.0);
            let diff = (a.h * height) / 2.0 - box_height / 2.0;
            let mut link_index = 0;
            for (index, line) in lines.iter().enumerate() {
                if !line.starts_with_inline() {
                    continue;
                }
                let Some(blob) = linked.get(link_index) else {
                    break;
                };
                let next_index = lines[index + 1..]
                    .iter()
                    .position(loco_pdf::edit::layout::Line::starts_with_inline)
                    .map_or(lines.len() - 1, |n| n + index);
                let url = if ctx.configs.with_file_links {
                    let mut blob = blob.clone();
                    crate::files::proxy_url(state, &mut blob, ctx.file_links_expire_at).await?
                } else {
                    format!("{}/e/{}", state.default_url(), form.submission.slug)
                };
                let shift = height_diff - if height_diff == 0.0 { diff } else { 0.0 };
                let above: f64 = heights[..index].iter().sum();
                let through: f64 = heights[..=next_index].iter().sum();
                editor.add_link(
                    page,
                    [
                        a.x * width + TEXT_LEFT_MARGIN,
                        height - a.y * height - above + shift,
                        a.x * width + a.w * width + TEXT_LEFT_MARGIN,
                        height - a.y * height - through + shift,
                    ],
                    &url,
                );
                link_index += 1;
            }
            let fitted = fit(
                layouter,
                items,
                a.w * width,
                if height_diff > 0.0 {
                    box_height
                } else {
                    a.h * height
                },
            );
            editor.draw_text(
                page,
                &fitted,
                a.x * width + TEXT_LEFT_MARGIN,
                height - a.y * height + height_diff - TEXT_TOP_MARGIN,
            );
        }
        "checkbox" | "multiple" | "radio"
            if field_type == "checkbox" || present(area.get("option_uuid")) =>
        {
            let mut checked = value == Some(Value::Bool(true));
            if matches!(field_type_raw, "multiple" | "radio") {
                let options = field.get("options").and_then(Value::as_array);
                let option = options.and_then(|o| {
                    o.iter().find(|o| {
                        o.get("uuid") == area.get("option_uuid")
                            && area.get("option_uuid").is_some()
                    })
                });
                checked = match (option, options) {
                    (Some(option), Some(options)) => {
                        let name = option
                            .get("value")
                            .filter(|v| present(Some(v)))
                            .map_or_else(
                                || {
                                    let index =
                                        options.iter().position(|o| o == option).unwrap_or(0) + 1;
                                    format!("{} {index}", state.views.t(&ctx.locale, "option", &[]))
                                },
                                ruby_to_s,
                            );
                        wrap(value.as_ref())
                            .iter()
                            .any(|v| v.as_str() == Some(name.as_str()))
                    }
                    _ => false,
                };
            }
            if !checked {
                return Ok(());
            }
            let check = editor
                .cached_image("check", || {
                    image::load_from_memory(CHECK_PNG).map_err(|e| EditError::Image(e.to_string()))
                })
                .map_err(pdf_error)?;
            let scale = (a.w * width / ICON_SIZE).min(a.h * height / ICON_SIZE);
            let name = editor.use_image(page, check.index);
            editor.overlay(page).image(
                &name,
                a.x * width + a.w * width / 2.0 - ICON_SIZE * scale / 2.0,
                height - a.y * height - a.h * height / 2.0 - ICON_SIZE * scale / 2.0,
                ICON_SIZE * scale,
                ICON_SIZE * scale,
            );
        }
        "cells" if area.get("cell_w").and_then(Value::as_f64).unwrap_or(0.0) != 0.0 => {
            let mut cell_width = num("cell_w") * width;
            let cell_layouter = Layouter::new(Align::Center, valign_of(field));
            let mut text = value_text.clone();
            if let Some(mask) = pref(field, "mask").filter(|m| present(Some(m))) {
                text = crate::email_variables::mask_value(&text, mask);
            }
            let mut chars: Vec<char> = maybe_rtl_reverse(&text)?.chars().collect();
            let right_aligned = pref_str(field, "align") == Some("right");
            if right_aligned {
                chars.reverse();
            }
            for (index, ch) in chars.iter().enumerate() {
                if ch.is_whitespace() {
                    continue;
                }
                let ch = ch.to_string();
                let mut fragment = Fragment::new(&ch, &style(font_size, None, fill_color));
                let line = fit(layouter, vec![Input::Text(fragment.clone())], width, height)
                    .lines
                    .into_iter()
                    .next();
                let mut line_height = line
                    .as_ref()
                    .map_or(0.0, loco_pdf::edit::layout::Line::height);
                cell_width = cell_width.max(
                    line.as_ref()
                        .map_or(0.0, loco_pdf::edit::layout::Line::width),
                );
                for divisor in [1.4, 1.9] {
                    if preferences_font_size.is_none() && line_height > a.h * height {
                        fragment = Fragment::new(
                            &ch,
                            &style((font_size / divisor).trunc(), None, fill_color),
                        );
                        line_height = fit(
                            layouter,
                            vec![Input::Text(fragment.clone())],
                            cell_width,
                            height,
                        )
                        .lines
                        .first()
                        .map_or(0.0, loco_pdf::edit::layout::Line::height);
                    }
                }
                #[allow(clippy::cast_precision_loss, reason = "cell index")]
                let i = index as f64;
                let x = if right_aligned {
                    (a.x + a.w) * width - cell_width * (i + 1.0)
                } else {
                    a.x * width + cell_width * i
                };
                let fitted = cell_layouter.fit(
                    vec![Input::Text(fragment)],
                    cell_width,
                    line_height.max(a.h * height),
                    &layouter_style,
                );
                editor.draw_text(page, &fitted, x, height - a.y * height);
            }
        }
        "strikethrough" => {
            let stroke = color(pref_str(field, "color")).unwrap_or([1.0, 0.0, 0.0]);
            let scale = 1000.0 / width;
            let line_width = 6.0 / scale;
            let area_height = a.h * height;
            let canvas = editor.overlay(page);
            if area_height * scale < 40.0 {
                let y = height - height * a.y - area_height / 2.0;
                canvas.line(
                    stroke,
                    line_width,
                    (width * a.x, y),
                    (width * a.x + width * a.w, y),
                );
            } else {
                let x1 = width * a.x + line_width / 2.0;
                let x2 = width * a.x + width * a.w - line_width / 2.0;
                let y1 = height - height * a.y - line_width / 2.0;
                let y2 = height - height * a.y - area_height + line_width / 2.0;
                canvas.line(stroke, line_width, (x1, y1), (x2, y2));
                canvas.line(stroke, line_width, (x1, y2), (x2, y1));
            }
        }
        _ => {
            let timezone = timezone(ctx);
            let mut text = match field_type_raw {
                "date" => crate::l10n::format_date_string(
                    state,
                    &value_text,
                    pref_str(field, "format"),
                    &ctx.locale,
                    &timezone,
                ),
                "number" => crate::l10n::format_number(
                    state,
                    value.as_ref().unwrap_or(&Value::Null),
                    pref_str(field, "format"),
                ),
                _ => wrap(value.as_ref())
                    .iter()
                    .map(ruby_to_s)
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            text = maybe_rtl_reverse(&text)?;
            if let Some(mask) = pref(field, "mask").filter(|m| present(Some(m))) {
                text = crate::email_variables::mask_value(&text, mask);
            }
            let is_date_or_number = matches!(field_type_raw, "date" | "number");
            let fit_width = if is_date_or_number {
                width
            } else {
                a.w * width
            };
            let mut size = font_size;
            let make = |text: &str, size: f64| {
                Fragment::new(
                    text,
                    &style(size, Some(size * line_height_factor), fill_color),
                )
            };
            let mut fragment = make(&text.replace('\u{A0}', " "), size);
            let mut box_height = fit(
                layouter,
                vec![Input::Text(fragment.clone())],
                a.w * width,
                height,
            )
            .lines_height();
            for divisor in [1.4, 1.9] {
                if preferences_font_size.is_none() && box_height > a.h * height + 1.0 {
                    size = (font_size / divisor).trunc();
                    fragment = make(&text, size);
                    box_height = fit(
                        layouter,
                        vec![Input::Text(fragment.clone())],
                        fit_width,
                        height,
                    )
                    .lines_height();
                }
            }
            let height_diff = (box_height - a.h * height).max(0.0);
            let right_align_x_adjustment = if is_date_or_number && text_align != Align::Left {
                (width - a.w * width)
                    / if text_align == Align::Center {
                        2.0
                    } else {
                        1.0
                    }
            } else {
                0.0
            };
            let align_y_diff = match vertical_align {
                Valign::Top => 0.0,
                Valign::Bottom => height_diff + TEXT_TOP_MARGIN,
                Valign::Center => height_diff / 2.0,
            };
            let fitted = fit(
                layouter,
                vec![Input::Text(fragment)],
                fit_width,
                if height_diff > 0.0 {
                    box_height
                } else {
                    a.h * height
                },
            );
            editor.draw_text(
                page,
                &fitted,
                a.x * width - right_align_x_adjustment + TEXT_LEFT_MARGIN,
                height - a.y * height + align_y_diff - TEXT_TOP_MARGIN,
            );
        }
    }
    Ok(())
}

/// The paperclip `InlineBox` in front of an attachment name.
fn paperclip_box(icon: ImageInfo, font_size: f64) -> InlineImage {
    InlineImage {
        image: icon.index,
        content_width: font_size,
        content_height: font_size,
        draw_height: font_size * f64::from(icon.height) / f64::from(icon.width),
        margin: [0.0, 1.0, -2.0, 0.0],
    }
}

/// The ID line, shortened with `...` until it fits on one line.
fn truncate_id(
    layouter: Layouter,
    empty: &Arc<TextStyle>,
    id_text: &str,
    style: &dyn Fn(f64, Option<f64>, Option<[f64; 3]>) -> Arc<TextStyle>,
    size: f64,
    width: f64,
) -> String {
    truncate_id_result(layouter, empty, id_text, style, size, width).0
}

fn truncate_id_result(
    layouter: Layouter,
    empty: &Arc<TextStyle>,
    id_text: &str,
    style: &dyn Fn(f64, Option<f64>, Option<[f64; 3]>) -> Arc<TextStyle>,
    size: f64,
    width: f64,
) -> (String, Fitted) {
    let mut id_string = id_text.to_owned();
    loop {
        let result = layouter.fit(
            vec![Input::Text(Fragment::new(
                &id_string,
                &style(size, None, None),
            ))],
            width,
            size / 0.65,
            empty,
        );
        if result.status == Status::Success {
            return (id_string, result);
        }
        let base = id_string.strip_suffix("...").unwrap_or(&id_string);
        let mut chars: Vec<char> = base.chars().collect();
        chars.pop();
        id_string = format!("{}...", chars.into_iter().collect::<String>());
        if id_string.chars().count() < 8 {
            return (id_string, result);
        }
    }
}

fn timezone(ctx: &Ctx<'_>) -> String {
    let account = &ctx.form.account.timezone;
    if ctx.configs.with_submitter_timezone {
        ctx.form
            .submitter
            .timezone
            .clone()
            .unwrap_or_else(|| account.clone())
    } else {
        account.clone()
    }
}

/// The signature's reason and time lines.
fn reason_string(
    ctx: &Ctx<'_>,
    field: &Value,
    reason_value: Option<&str>,
    attachment_created_at: NaiveDateTime,
) -> Result<String, AppError> {
    let state = ctx.state;
    let submitter = &ctx.form.submitter;
    let timezone = timezone(ctx);
    let format = if ctx.configs.with_timestamp_seconds {
        "detailed"
    } else {
        "long"
    };
    let timestamp = if ctx.configs.with_signature_id_completed_at {
        submitter.completed_at.unwrap_or(attachment_created_at)
    } else {
        attachment_created_at
    };
    let time = format!(
        "{} {}",
        crate::l10n::time(state, &ctx.locale, timestamp, &timezone, format),
        crate::l10n::timezone_abbr(&timezone, timestamp)
    );
    if ctx.configs.with_signature_id_reason || present(pref(field, "reasons")) {
        let t = |key: &str| state.views.t(&ctx.locale, key, &[]);
        let prefix = reason_value
            .map(|_| format!("{}: ", t("reason")))
            .unwrap_or_default();
        let reason = reason_value.map_or_else(|| t("digitally_signed_by"), str::to_owned);
        let name = submitter.name.clone().unwrap_or_default();
        let email = submitter
            .email
            .as_deref()
            .filter(|e| !e.is_empty())
            .map(|e| format!(" <{e}>"))
            .unwrap_or_default();
        let line = maybe_rtl_reverse(&format!("{prefix}{reason} {name}{email}"))?;
        Ok(format!("{line}\n{time}"))
    } else {
        Ok(time)
    }
}

/// What documents are signed with (`build_signing_params(submitter, pkcs, tsa_url)`).
pub(super) struct Signer {
    pub reason: String,
    pub key: Arc<SigningKey>,
    /// `timestamp_handler: TimestampHandler.new(tsa_url:)`.
    pub timestamp: Option<Arc<TimestampHandler>>,
}

/// `build_pdf_attachment`, first half: the document saved as an incremental update, signed
/// when there is a signer, with `PDF_FORMAT=pdf/a-3b` as PDF/A-3b.
async fn save_pdf(
    state: &AppState,
    mut editor: Editor,
    signer: Option<&Signer>,
) -> Result<Vec<u8>, AppError> {
    editor.set_info_text("Creator", "DocuSeal (https://www.docuseal.com)");
    if state.config.pdf_format == PDF_A_3B {
        editor.set_pdfa(Pdfa {
            conformance: 'B',
            fix_glyph_widths: true,
        });
    }
    save_document(editor, signer).await
}

/// `pdf.sign(io, reason:, **signing_params)` or `pdf.write(io, incremental: true)`, off the
/// async workers: signing is CPU-bound, and the timestamp server is asked from within it.
pub(super) async fn save_document(
    editor: Editor,
    signer: Option<&Signer>,
) -> Result<Vec<u8>, AppError> {
    let signer = signer.map(|s| (s.reason.clone(), s.key.clone(), s.timestamp.clone()));
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let now = Utc::now();
        let date = now.format("D:%Y%m%d%H%M%S").to_string();
        let Some((reason, key, timestamp)) = signer else {
            return editor.save(&date);
        };
        let mut stamp = timestamp
            .map(|handler| move |signature: &[u8]| runtime.block_on(handler.sign(signature)));
        editor.save_signed(
            &date,
            &now.format("%Y%m%d%H%M%S").to_string(),
            &reason,
            &key,
            stamp
                .as_mut()
                .map(|f| f as loco_pdf::edit::sign::Timestamper<'_>),
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("saving the document: {e}"))?
    .map_err(pdf_error)
}

/// `build_pdf_attachment`, second half: the saved document attached to the submitter as
/// `"#{name}.pdf"`, with its schema uuid as `original_uuid`.
async fn store_pdf(
    state: &AppState,
    submitter: &Submitter,
    bytes: Vec<u8>,
    uuid: &str,
    name: &str,
) -> Result<(Attachment, Blob), AppError> {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};
    let sha256 = base64::engine::general_purpose::URL_SAFE.encode(Sha256::digest(&bytes));
    let filename = format!("{name}.pdf").replace('/', "-");
    let blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            bytes.into(),
            &filename,
            Some("application/pdf"),
            json!({ "original_uuid": uuid, "analyzed": true, "sha256": sha256 }),
        )
        .await?;
    let attachment =
        Attachment::create(&state.db, "Submitter", submitter.id, "documents", &blob).await?;
    Ok((attachment, blob))
}

mod lopdf_id {
    pub type ObjectId = (u32, u16);
}
