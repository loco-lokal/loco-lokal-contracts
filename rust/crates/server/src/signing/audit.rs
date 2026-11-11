//! `Submissions::EnsureAuditGenerated` and `Submissions::GenerateAuditTrail`: the audit
//! log PDF of a completed submission (documents with their checksums, the parties with
//! their verification details and values, and the event log), signed like the result
//! documents and attached to the submission as `audit_trail`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{Datelike as _, NaiveDate, NaiveDateTime, Utc};
use loco_db::{
    signing::{self, SubmitterVersion},
    submissions::{self, SubmissionEvent, Submitter},
    users,
};
use loco_domain::submit::{blank, present};
use loco_pdf::edit::{
    EditError, Editor,
    compose::{BoxStyle, ColumnBox, Composer, Content, ImageBox, Quad, RuleBox, TableBox, TextBox},
    fonts::FontData,
    layout::{Align, Fragment, Input, Layouter, TextStyle, Valign},
};
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};

use super::{
    Form,
    results::{
        FONT_BOLD_NAME, FONT_NAME, SIGN_REASON, TESTING_FOOTER, TIMESTAMP_SERVER_URL_KEY,
        UNSUPPORTED_IMAGE_TYPES, Uploads, WITH_FILE_LINKS_KEY, WITH_SIGNATURE_ID,
        WITH_SUBMITTER_TIMEZONE_KEY, WITH_TIMESTAMP_SECONDS_KEY, default_font, is_rtl,
        load_signing_key, ruby_to_s, unsupported, wrap,
    },
};
use crate::{errors::AppError, state::AppState};

const FONT_SIZE: f64 = 9.0;
const TEXT_COLOR: [f64; 3] = [82.0 / 255.0, 82.0 / 255.0, 82.0 / 255.0];
/// `hp-blue-light`.
const LINK_COLOR: [f64; 3] = [86.0 / 255.0, 176.0 / 255.0, 1.0];
/// `hp-gray-light`.
const DIVIDER_COLOR: [f64; 3] = [224.0 / 255.0, 224.0 / 255.0, 224.0 / 255.0];
const BACKGROUND_COLOR: [f64; 3] = [250.0 / 255.0, 247.0 / 255.0, 245.0 / 255.0];
const MAX_IMAGE_HEIGHT: f64 = 100.0;
const CHECKSUM_LIMIT: usize = 30;
const US_TIMEZONES: &[&str] = &[
    "EST", "EDT", "CST", "CDT", "MST", "MDT", "PST", "PDT", "HST", "HDT", "AKST", "AKDT",
];
const A4: (f64, f64) = (595.275_591, 841.889_764);
const LETTER: (f64, f64) = (612.0, 792.0);
const LOGO_PNG: &[u8] = include_bytes!("../../../../../lib/pdf_icons/logo.png");
const SRGB_ICC: &[u8] = include_bytes!("../../../../data/icc/sRGB2014.icc");

const WITH_AUDIT_VALUES_KEY: &str = "with_audit_values";
const WITH_AUDIT_SENDER_KEY: &str = "with_audit_sender";
pub(super) const COMBINE_PDF_RESULT_KEY: &str = "combine_pdf_result_key";

/// `EnsureAuditGenerated::CHECK_COMPLETE_TIMEOUT`.
const CHECK_COMPLETE_TIMEOUT: Duration = Duration::from_secs(90);

#[allow(clippy::needless_pass_by_value, reason = "used with map_err")]
fn pdf_error(err: EditError) -> AppError {
    anyhow::anyhow!("audit trail: {err}").into()
}

/// `account.account_configs.exists?(key: COMBINE_PDF_RESULT_KEY, value: true)`: combined
/// results are not ported.
pub async fn check_combined(state: &AppState, account_id: i64) -> Result<(), AppError> {
    let rows = signing::account_configs(&state.db, account_id, &[COMBINE_PDF_RESULT_KEY]).await?;
    if rows.iter().any(|(_, v)| *v == Value::Bool(true)) {
        return Err(unsupported("Combining the result documents into one PDF"));
    }
    Ok(())
}

/// `Submissions::EnsureAuditGenerated.call(submission)`: the audit trail attachment,
/// generated once (other workers wait for the one generating).
pub async fn ensure_audit_generated(
    state: &AppState,
    submission_id: i64,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    let submission = submissions::find(&state.db, submission_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("submission {submission_id} missing"))?;
    if submission.completed_at.is_none() {
        return Err(anyhow::anyhow!("submission {submission_id} has not completed yet").into());
    }
    let key = format!("audit_trail:{submission_id}");
    let started = std::time::Instant::now();
    loop {
        let events = signing::lock_event_names(&state.db, &key).await?;
        if events.iter().any(|e| e == "complete") {
            return audit_trail(state, submission_id).await;
        }
        if events.last().is_some_and(|e| e == "start" || e == "retry") {
            return wait_for_complete_or_fail(state, submission_id, &key).await;
        }
        let name = if events.is_empty() { "start" } else { "retry" };
        if !signing::create_lock_event(&state.db, &key, name).await? {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
                return Err(anyhow::anyhow!("timed out waiting for {key}").into());
            }
            continue;
        }
        return match Box::pin(generate(state, submission_id)).await {
            Ok(attachment) => {
                signing::create_lock_event(&state.db, &key, "complete").await?;
                Ok(Some(attachment))
            }
            Err(err) => {
                tracing::error!(error = ?err, submission_id, "generating the audit trail failed");
                signing::create_lock_event(&state.db, &key, "fail").await?;
                Err(err)
            }
        };
    }
}

async fn wait_for_complete_or_fail(
    state: &AppState,
    submission_id: i64,
    key: &str,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = signing::lock_event_names(&state.db, key).await?;
        if events
            .last()
            .is_some_and(|e| e == "complete" || e == "fail")
        {
            return audit_trail(state, submission_id).await;
        }
        if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
            return Err(anyhow::anyhow!("timed out waiting for {key}").into());
        }
    }
}

async fn audit_trail(
    state: &AppState,
    submission_id: i64,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    Ok(
        Attachment::with_blobs_for_records(
            &state.db,
            "Submission",
            &[submission_id],
            "audit_trail",
        )
        .await?
        .into_iter()
        .next(),
    )
}

/// The account configs `build_audit_trail` reads.
struct Configs {
    with_signature_id: bool,
    with_file_links: bool,
    with_audit_values: bool,
    with_audit_sender: bool,
    with_submitter_timezone: bool,
    with_timestamp_seconds: bool,
}

impl Configs {
    async fn load(state: &AppState, account_id: i64) -> Result<Self, AppError> {
        let rows = signing::account_configs(
            &state.db,
            account_id,
            &[
                WITH_AUDIT_VALUES_KEY,
                WITH_SIGNATURE_ID,
                WITH_FILE_LINKS_KEY,
                WITH_TIMESTAMP_SECONDS_KEY,
                WITH_AUDIT_SENDER_KEY,
                WITH_SUBMITTER_TIMEZONE_KEY,
            ],
        )
        .await?;
        let find = |key: &str| rows.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        let is_true = |key: &str| find(key) == Some(&Value::Bool(true));
        Ok(Self {
            with_signature_id: is_true(WITH_SIGNATURE_ID),
            with_file_links: is_true(WITH_FILE_LINKS_KEY),
            with_audit_values: find(WITH_AUDIT_VALUES_KEY) != Some(&Value::Bool(false)),
            with_audit_sender: is_true(WITH_AUDIT_SENDER_KEY),
            with_submitter_timezone: is_true(WITH_SUBMITTER_TIMEZONE_KEY),
            with_timestamp_seconds: is_true(WITH_TIMESTAMP_SECONDS_KEY),
        })
    }
}

/// `TextUtils.maybe_rtl_reverse`: right-to-left text needs a Bidi reordering that is not
/// ported, so it fails the job instead of drawing differently.
fn ltr(text: &str) -> Result<String, AppError> {
    if is_rtl(text) {
        return Err(unsupported("Right-to-left text in the audit trail"));
    }
    Ok(text.to_owned())
}

/// `String#squish`.
fn squish(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `String#titleize` for the checkbox values (`true`/`false`).
fn titleize(text: &str) -> String {
    text.split(|c: char| c == '_' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ruby `a || b` for optional strings: an empty string is still a value.
fn or<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.or(b)
}

/// A piece of formatted text.
enum Piece {
    Regular(String),
    Bold(String),
    Link { text: String, uri: String },
}

/// The fonts and styles of the audit trail (`composer.style(:base, …)`, `:link`).
struct Styles {
    regular: (Arc<FontData>, String),
    bold: (Arc<FontData>, String),
}

impl Styles {
    fn load(editor: &mut Editor) -> Result<Self, AppError> {
        let regular = editor.font(default_font(), "none").map_err(pdf_error)?;
        let bold = if default_font() == FONT_NAME {
            editor.font(FONT_BOLD_NAME, "none")
        } else {
            editor.font("Helvetica", "bold")
        }
        .map_err(pdf_error)?;
        Ok(Self { regular, bold })
    }

    fn style(&self, bold: bool, size: f64, link: Option<&str>) -> Arc<TextStyle> {
        let (font, key) = if bold { &self.bold } else { &self.regular };
        Arc::new(TextStyle {
            font: font.clone(),
            font_key: key.clone(),
            font_size: size,
            line_height: None,
            fill_color: Some(if link.is_some() {
                LINK_COLOR
            } else {
                TEXT_COLOR
            }),
            white_underlay: false,
            underline: link.is_some(),
            link: link.map(str::to_owned),
        })
    }

    /// `formatted_text_box(pieces, font_size:, line_spacing:, text_align:, …)`.
    fn text(
        &self,
        pieces: Vec<Piece>,
        size: f64,
        line_spacing: f64,
        align: Align,
        style: BoxStyle,
    ) -> Content {
        let items = pieces
            .into_iter()
            .filter_map(|piece| {
                let (text, style) = match piece {
                    Piece::Regular(text) => (text, self.style(false, size, None)),
                    Piece::Bold(text) => (text, self.style(true, size, None)),
                    Piece::Link { text, uri } => (text, self.style(false, FONT_SIZE, Some(&uri))),
                };
                (!text.is_empty()).then(|| Input::Text(Fragment::new(&text, &style)))
            })
            .collect();
        Content::Text(TextBox::new(
            items,
            Layouter::new(align, Valign::Top).with_line_spacing(line_spacing),
            self.style(false, size, None),
            style,
        ))
    }

    fn plain(&self, text: &str) -> Content {
        self.text(
            vec![Piece::Regular(text.to_owned())],
            FONT_SIZE,
            1.0,
            Align::Left,
            BoxStyle::default(),
        )
    }
}

fn padding(top: f64, right: f64, bottom: f64, left: f64) -> BoxStyle {
    BoxStyle::default().padding(Quad::new(top, right, bottom, left))
}

fn divider() -> Content {
    Content::Rule(RuleBox {
        style: BoxStyle::default().margin(Quad::new(0.0, 0.0, 15.0, 0.0)),
        height: 1.0,
        border_width: 1.0,
        color: DIVIDER_COLOR,
    })
}

/// `TimeUtils.timezone_abbr(account.timezone, Time.current.beginning_of_year)`.
fn page_size(timezone: &str) -> (f64, f64) {
    let year = Utc::now().year();
    let start = NaiveDate::from_ymd_opt(year, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or_default();
    if US_TIMEZONES.contains(&crate::l10n::timezone_abbr(timezone, start).as_str()) {
        LETTER
    } else {
        A4
    }
}

/// `settings_esign_url` on `EMAIL_HOST`, else the app's host.
fn verify_url(state: &AppState) -> String {
    let base = state.default_url();
    let Some(host) = std::env::var("EMAIL_HOST").ok().filter(|h| !h.is_empty()) else {
        return format!("{base}/settings/esign");
    };
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", &base));
    let authority = rest.split('/').next().unwrap_or(rest);
    let port = authority
        .rsplit_once(':')
        .map(|(_, port)| format!(":{port}"))
        .unwrap_or_default();
    format!("{scheme}://{host}{port}/settings/esign")
}

/// Everything the layout needs, loaded up front.
struct Data<'a> {
    state: &'a AppState,
    form: Form,
    configs: Configs,
    /// `I18n.locale` (the last submitter's `lang`, else the account's).
    locale: String,
    timezone: String,
    events: Vec<SubmissionEvent>,
    versions: HashMap<i64, Vec<SubmitterVersion>>,
    uploads: HashMap<i64, Uploads>,
}

impl Data<'_> {
    fn t(&self, key: &str) -> String {
        self.state.views.t(&self.locale, key, &[])
    }

    fn t_with(&self, key: &str, vars: &[(&str, String)]) -> String {
        self.state.views.t(&self.locale, key, vars)
    }

    /// `I18n.l(time.in_time_zone(timezone), format:, locale: account.locale)` and the zone.
    fn time(&self, utc: NaiveDateTime, format: &str) -> String {
        format!(
            "{} {}",
            crate::l10n::time(
                self.state,
                &self.form.account.locale,
                utc,
                &self.timezone,
                format
            ),
            crate::l10n::timezone_abbr(&self.timezone, utc)
        )
    }

    fn submitter(&self, id: i64) -> Option<&Submitter> {
        self.form.submitters.iter().find(|s| s.id == id)
    }
}

/// `GenerateAuditTrail.call(submission)`.
async fn generate(state: &AppState, submission_id: i64) -> Result<(Attachment, Blob), AppError> {
    let submitters = submissions::submitters_for(&state.db, &[submission_id]).await?;
    let last = submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("submission {submission_id} has no completed party"))?;
    let form = Form::for_submitter(state, last).await?;
    let configs = Configs::load(state, form.account.id).await?;
    let locale = form
        .submitter
        .metadata
        .0
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or(&form.account.locale)
        .to_owned();
    let timezone = if configs.with_submitter_timezone {
        form.submitter
            .timezone
            .clone()
            .unwrap_or_else(|| form.account.timezone.clone())
    } else {
        form.account.timezone.clone()
    };
    let events = submissions::events_for_submissions(&state.db, &[submission_id]).await?;
    let ids: Vec<i64> = form.submitters.iter().map(|s| s.id).collect();
    let mut versions: HashMap<i64, Vec<SubmitterVersion>> = HashMap::new();
    for version in signing::submitter_versions(&state.db, &ids).await? {
        versions
            .entry(version.submitter_id)
            .or_default()
            .push(version);
    }
    let mut uploads = HashMap::new();
    for submitter in &form.submitters {
        let items = Attachment::with_blobs_for_records(
            &state.db,
            "Submitter",
            &[submitter.id],
            "attachments",
        )
        .await?;
        uploads.insert(
            submitter.id,
            Uploads {
                items,
                data: HashMap::new(),
            },
        );
    }
    let mut data = Data {
        state,
        form,
        configs,
        locale,
        timezone,
        events,
        versions,
        uploads,
    };

    let key = load_signing_key(state, data.form.account.id).await?;
    if key.is_some()
        && signing::encrypted_config_with_fallback(
            &state.db,
            data.form.account.id,
            TIMESTAMP_SERVER_URL_KEY,
        )
        .await?
        .and_then(|v| state.secrets.encryptor.decrypt(&v).ok())
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .is_some_and(|v| present(Some(&v)))
    {
        return Err(unsupported(
            "Timestamping signatures with a timestamp server",
        ));
    }

    let mut editor = Editor::blank().map_err(pdf_error)?;
    build(&mut data, &mut editor).await?;

    let now = Utc::now();
    editor.set_info_text("Creator", "DocuSeal (https://www.docuseal.com)");
    if default_font() == FONT_NAME {
        add_pdfa(&mut editor, now).map_err(pdf_error)?;
    }
    let pdf_date = now.format("D:%Y%m%d%H%M%S").to_string();
    let bytes = match &key {
        Some(key) => editor
            .save_signed(
                &pdf_date,
                &now.format("%Y%m%d%H%M%S").to_string(),
                SIGN_REASON,
                key,
            )
            .map_err(pdf_error)?,
        None => editor.save(&pdf_date).map_err(pdf_error)?,
    };

    let name = data
        .form
        .submission
        .name
        .clone()
        .or_else(|| data.form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default();
    let filename = format!("{} - {name}.pdf", data.t("audit_log")).replace('/', "-");
    let blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            bytes.into(),
            &filename,
            Some("application/pdf"),
            json!({ "identified": true, "analyzed": true }),
        )
        .await?;
    let attachment =
        Attachment::create(&state.db, "Submission", submission_id, "audit_trail", &blob).await?;
    Ok((attachment, blob))
}

/// The `:pdfa` task (level 3u): XMP metadata and the sRGB output intent.
fn add_pdfa(editor: &mut Editor, now: chrono::DateTime<Utc>) -> Result<(), EditError> {
    use loco_pdf::edit::lopdf::{Dictionary, Object, Stream};
    let packet_id = uuid::Uuid::new_v4().simple().to_string();
    let xmp = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"{packet_id}\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
         <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n\
         <xmp:CreatorTool>DocuSeal (https://www.docuseal.com)</xmp:CreatorTool>\n\
         <xmp:ModifyDate>{}</xmp:ModifyDate>\n\
         </rdf:Description>\n\
         <rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n\
         <pdfaid:part>3</pdfaid:part>\n\
         <pdfaid:conformance>U</pdfaid:conformance>\n\
         </rdf:Description>\n\
         <rdf:Description rdf:about=\"\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">\n\
         <pdf:Producer>{}</pdf:Producer>\n\
         </rdf:Description>\n\
         </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"r\"?>",
        now.format("%Y-%m-%dT%H:%M:%S+00:00"),
        loco_pdf::edit::PRODUCER
    );
    let mut meta = Dictionary::new();
    meta.set("Type", Object::Name(b"Metadata".to_vec()));
    meta.set("Subtype", Object::Name(b"XML".to_vec()));
    let metadata = editor.add_object(Object::Stream(
        Stream::new(meta, xmp.into_bytes()).with_compression(false),
    ));
    let mut icc = Dictionary::new();
    icc.set("N", Object::Integer(3));
    let profile = editor.add_object(Object::Stream(
        Stream::new(icc, SRGB_ICC.to_vec()).with_compression(false),
    ));
    let mut intent = Dictionary::new();
    intent.set("S", Object::Name(b"GTS_PDFA1".to_vec()));
    intent.set(
        "OutputConditionIdentifier",
        Object::string_literal("sRGB2014.icc"),
    );
    intent.set("Info", Object::string_literal("sRGB2014.icc"));
    intent.set(
        "RegistryName",
        Object::string_literal("https://www.color.org"),
    );
    intent.set("DestOutputProfile", Object::Reference(profile));
    let intent = editor.add_object(Object::Dictionary(intent));
    editor.set_catalog(
        "OutputIntents",
        Object::Array(vec![Object::Reference(intent)]),
    )?;
    editor.set_catalog("Metadata", Object::Reference(metadata))
}

/// `build_audit_trail(submission)`: lays the document out.
#[allow(clippy::too_many_lines)]
async fn build(data: &mut Data<'_>, editor: &mut Editor) -> Result<(), AppError> {
    let state = data.state;
    let styles = Styles::load(editor)?;
    let account = data.form.account.clone();
    let submission = data.form.submission.clone();

    // `page_style(:default)`: the background bands and the Document ID footer.
    let footer = if data.configs.with_signature_id || account.testing {
        use md5::{Digest as _, Md5};
        let document_id =
            Md5::digest(submission.slug.as_bytes())
                .iter()
                .fold(String::new(), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(acc, "{b:02X}");
                    acc
                });
        Some(if account.testing {
            if data.configs.with_signature_id {
                format!("{TESTING_FOOTER} | ID: {document_id}")
            } else {
                TESTING_FOOTER.to_owned()
            }
        } else {
            format!("{}: {document_id}", data.t("document_id"))
        })
    } else {
        None
    };
    let size = page_size(&account.timezone);
    let logo = editor
        .add_image(
            &image::load_from_memory(LOGO_PNG).map_err(|e| anyhow::anyhow!("logo: {e}"))?,
            false,
        )
        .map_err(pdf_error)?;

    // Boxes are built first (they need the database), then drawn page by page.
    let mut boxes: Vec<Content> = Vec::new();

    let title = if account.testing {
        data.t("testing_log_not_for_production_use")
    } else {
        data.t("audit_log")
    };
    boxes.push(Content::Column(ColumnBox {
        style: BoxStyle::default(),
        children: vec![
            Content::Image(ImageBox {
                style: BoxStyle::default().float(),
                image: logo.index,
                width: 40.0,
                height: 40.0,
            }),
            match styles.text(
                vec![Piece::Bold("DocuSeal".to_owned())],
                20.0,
                1.0,
                Align::Left,
                padding(5.0, 0.0, 0.0, 8.0).float(),
            ) {
                Content::Text(text) => Content::Text(text.width(100.0)),
                other => other,
            },
            styles.text(
                vec![Piece::Regular(title)],
                16.0,
                1.0,
                Align::Right,
                padding(10.0, 0.0, 0.0, 0.0).float(),
            ),
        ],
    }));

    let mut header = vec![styles.text(
        vec![Piece::Regular(format!(
            "{}: {}",
            data.t("envelope_id"),
            submission.id
        ))],
        12.0,
        1.0,
        Align::Left,
        padding(15.0, 0.0, 8.0, 0.0).float(),
    )];
    if !matches!(submission.source.as_str(), "embed" | "api") {
        header.push(styles.text(
            vec![Piece::Link {
                text: data.t("verify"),
                uri: verify_url(state),
            }],
            9.0,
            1.0,
            Align::Right,
            padding(15.0, 0.0, 10.0, 0.0).float(),
        ));
    }
    boxes.push(Content::Column(ColumnBox {
        style: BoxStyle::default(),
        children: header,
    }));
    boxes.push(divider());

    // Documents with their checksums.
    let documents = select_attachments(state, &data.form).await?;
    let schema_documents = super::pages::schema_documents(state, &data.form)
        .await?
        .unwrap_or_default();
    let mut rows = Vec::new();
    for (attachment, blob) in &documents {
        let meta = blob.metadata_json();
        let original_uuid = meta
            .get("original_uuid")
            .and_then(Value::as_str)
            .unwrap_or(&attachment.uuid);
        let mut originals: Vec<&Blob> = schema_documents
            .iter()
            .filter(|d| d.attachment.uuid == original_uuid)
            .map(|d| &d.blob)
            .collect();
        if originals.is_empty() {
            originals = schema_documents
                .iter()
                .filter(|d| {
                    d.blob
                        .content_type
                        .as_deref()
                        .is_some_and(|t| t.starts_with("image/"))
                })
                .map(|d| &d.blob)
                .collect();
        }
        let checksum = |blob: &Blob| {
            blob.metadata_json()
                .get("sha256")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| blob.checksum.clone())
                .unwrap_or_default()
        };
        let original_sums = originals
            .iter()
            .take(CHECKSUM_LIMIT)
            .map(|b| checksum(b))
            .collect::<Vec<_>>()
            .join("\n");
        rows.push(vec![
            Some(styles.text(
                vec![Piece::Regular(ltr(&blob.filename)?)],
                FONT_SIZE,
                1.0,
                Align::Left,
                BoxStyle::default(),
            )),
            Some(styles.text(
                vec![
                    Piece::Bold(format!("{}:\n", data.t("original_sha256"))),
                    Piece::Regular(original_sums),
                    Piece::Regular("\n".to_owned()),
                    Piece::Bold(format!("{}:\n", data.t("result_sha256"))),
                    Piece::Regular(checksum(blob)),
                    Piece::Regular("\n".to_owned()),
                    Piece::Bold(format!("{}: ", data.t("generated_at"))),
                    Piece::Regular(data.time(attachment.created_at, "long")),
                ],
                FONT_SIZE,
                1.3,
                Align::Left,
                BoxStyle::default(),
            )),
        ]);
    }
    if !rows.is_empty() {
        boxes.push(Content::Table(TableBox::new(
            rows,
            Quad::new(0.0, 0.0, 20.0, 0.0),
        )));
        boxes.push(divider());
    }

    // The parties.
    let template_submitters = data.form.template_submitters().to_vec();
    let fields = data.form.fields().to_vec();
    let file_links_expire_at = if data.configs.with_file_links {
        crate::files::link_expires_at(state, account.id).await?
    } else {
        None
    };
    for item in &template_submitters {
        let item_uuid = item.get("uuid").and_then(Value::as_str).unwrap_or_default();
        let Some(submitter) = data
            .form
            .submitters
            .iter()
            .find(|s| s.uuid == item_uuid)
            .cloned()
        else {
            continue;
        };
        submitter_boxes(
            data,
            editor,
            &styles,
            &mut boxes,
            item,
            &submitter,
            template_submitters.len(),
            &fields,
            file_links_expire_at,
        )
        .await?;
    }

    boxes.push(divider());
    boxes.push(styles.text(
        vec![Piece::Regular(data.t("event_log"))],
        12.0,
        1.0,
        Align::Left,
        padding(10.0, 0.0, 20.0, 0.0),
    ));
    let event_rows = event_rows(data, &styles).await?;
    if !event_rows.is_empty() {
        boxes.push(Content::Table(TableBox::new(
            event_rows,
            Quad::new(0.0, 0.0, 12.0, 0.0),
        )));
    }

    let footer_style = Arc::new(TextStyle {
        font: styles.regular.0.clone(),
        font_key: styles.regular.1.clone(),
        font_size: FONT_SIZE,
        line_height: None,
        fill_color: None,
        white_underlay: true,
        underline: false,
        link: None,
    });
    let mut composer = Composer::new(editor, size, 50.0, move |editor, page| {
        let (width, height) = size;
        let canvas = editor.overlay(page);
        canvas.fill_rect(BACKGROUND_COLOR, 0.0, 0.0, width, 20.0);
        canvas.fill_rect(BACKGROUND_COLOR, 0.0, height - 20.0, width, 20.0);
        if let Some(text) = &footer {
            let fitted = Layouter::new(Align::Left, Valign::Top).fit(
                vec![Input::Text(Fragment::new(text, &footer_style))],
                1000.0,
                1000.0,
                &footer_style,
            );
            editor.draw_text(page, &fitted, 1.0, FONT_SIZE * 1.37);
        }
        Ok(())
    });
    composer.new_page().map_err(pdf_error)?;
    for content in boxes {
        composer.draw_box(content).map_err(pdf_error)?;
    }
    Ok(())
}

/// `Submitters.select_attachments_for_download(submitter)`: the result documents,
/// without image documents' pages when there are several images. Combined results are
/// rejected by `check_combined` before.
pub(super) async fn select_attachments(
    state: &AppState,
    form: &Form,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    let documents = Attachment::with_blobs_for_records(
        &state.db,
        "Submitter",
        &[form.submitter.id],
        "documents",
    )
    .await?;
    let originals = super::pages::schema_documents(state, form)
        .await?
        .unwrap_or_default();
    let is_image = |blob: &Blob| {
        blob.content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("image/"))
    };
    let many_images = originals.iter().filter(|d| is_image(&d.blob)).count() > 1;
    Ok(documents
        .into_iter()
        .filter(|(attachment, blob)| {
            if !many_images {
                return true;
            }
            let meta = blob.metadata_json();
            let uuid = meta
                .get("original_uuid")
                .and_then(Value::as_str)
                .unwrap_or(&attachment.uuid);
            !originals
                .iter()
                .find(|d| d.attachment.uuid == uuid)
                .is_some_and(|d| is_image(&d.blob))
        })
        .collect())
}

/// One party: contact and verification details, then each value.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn submitter_boxes(
    data: &mut Data<'_>,
    editor: &mut Editor,
    styles: &Styles,
    boxes: &mut Vec<Content>,
    item: &Value,
    submitter: &Submitter,
    roles: usize,
    fields: &[Value],
    file_links_expire_at: Option<chrono::DateTime<Utc>>,
) -> Result<(), AppError> {
    let state = data.state;
    let mut events: Vec<&SubmissionEvent> = data
        .events
        .iter()
        .filter(|e| e.submitter_id == Some(submitter.id))
        .collect();
    if let Some(delegated) = events
        .iter()
        .filter(|e| e.event_type == "delegate_form")
        .max_by_key(|e| e.event_timestamp)
        .map(|e| e.event_timestamp)
    {
        events.retain(|e| e.event_timestamp > delegated);
    }
    let find = |t: &str| events.iter().find(|e| e.event_type == t).copied();
    let completed = find("complete_form").map(|e| e.data.0.clone());
    let completed = completed.unwrap_or(Value::Null);
    let click_email = find("click_email").is_some();
    let email_verified = find("email_verified").is_some();
    let phone_verified = find("phone_verified").is_some();
    let id_verified = find("complete_verification").is_some();
    let kba = find("complete_kba");

    let mut contact = Vec::new();
    if roles > 1 {
        contact.push(Piece::Regular(format!(
            "{}\n",
            ltr(&item.get("name").map(ruby_to_s).unwrap_or_default())?
        )));
    }
    if let Some(email) = &submitter.email {
        contact.push(Piece::Bold(format!("{email}\n")));
    }
    if let Some(name) = &submitter.name {
        contact.push(Piece::Regular(format!("{}\n", ltr(name)?)));
    }
    if let Some(phone) = &submitter.phone {
        contact.push(Piece::Regular(format!("{phone}\n")));
    }
    let verified = data.t("verified");
    let mut details = Vec::new();
    if data.form.is_viewer(submitter) {
        details.push(format!("{}\n", data.t("view_only")));
    }
    if submitter.email.is_some() && (click_email || email_verified) {
        details.push(format!("{}: {verified}\n", data.t("email_verification")));
    }
    if submitter.phone.is_some() && phone_verified {
        details.push(format!("{}: {verified}\n", data.t("phone_verification")));
    }
    if id_verified {
        details.push(format!("{}: {verified}\n", data.t("identity_verification")));
    }
    if let Some(kba) = kba {
        let score = kba
            .data
            .0
            .get("answers")
            .filter(|a| !a.is_null())
            .map(|a| {
                format!(
                    " ({}/{})",
                    a.get("correct").map(ruby_to_s).unwrap_or_default(),
                    a.get("asked").map(ruby_to_s).unwrap_or_default()
                )
            })
            .unwrap_or_default();
        details.push(format!(
            "{}: {}{score}\n",
            data.t("knowledge_based_authentication"),
            data.t("passed")
        ));
    }
    let completed_value = |key: &str| {
        completed
            .get(key)
            .filter(|v| !v.is_null() && *v != &Value::Bool(false))
    };
    if let Some(ip) = completed_value("ip") {
        details.push(format!("IP: {}\n", ruby_to_s(ip)));
    }
    if let Some(sid) = completed_value("sid") {
        details.push(format!("{}: {}\n", data.t("session_id"), ruby_to_s(sid)));
    }
    if let Some(ua) = completed_value("ua") {
        details.push(format!("User agent: {}\n", squish(&ruby_to_s(ua))));
    }
    if let Some(timezone) = &submitter.timezone {
        details.push(format!(
            "Time zone: {}\n",
            timezone.replacen("Kiev", "Kyiv", 1)
        ));
    }
    // `compact_blank` drops blank items (an empty string, and the trailing "\n").
    let details: Vec<Piece> = details
        .into_iter()
        .filter(|d| !d.trim().is_empty())
        .map(Piece::Regular)
        .collect();
    let contact: Vec<Piece> = contact
        .into_iter()
        .filter(|p| match p {
            Piece::Regular(t) | Piece::Bold(t) => !t.trim().is_empty() || t.is_empty(),
            Piece::Link { .. } => true,
        })
        .collect();
    boxes.push(Content::Table(TableBox::new(
        vec![
            vec![Some(styles.text(
                contact,
                FONT_SIZE,
                1.3,
                Align::Left,
                padding(0.0, 20.0, 0.0, 0.0),
            ))],
            vec![Some(styles.text(
                details,
                FONT_SIZE,
                1.3,
                Align::Left,
                padding(10.0, 20.0, 20.0, 0.0),
            ))],
        ],
        Quad::default(),
    )));

    let values = submitter.values.0.clone();
    let field_type = |field: &Value| {
        field
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let is_for = |field: &Value| {
        field.get("submitter_uuid").and_then(Value::as_str) == Some(submitter.uuid.as_str())
    };
    let value_of = |field: &Value| {
        field
            .get("uuid")
            .and_then(Value::as_str)
            .and_then(|u| values.get(u))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let field_title = |field: &Value| {
        [field.get("title"), field.get("name")]
            .into_iter()
            .flatten()
            .find(|v| present(Some(v)))
            .map(ruby_to_s)
    };

    // Signatures and initials with the same value are listed once, under all names.
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut grouped: Vec<(Value, String)> = Vec::new();
    let mut skip: Vec<String> = Vec::new();
    for field in fields {
        let kind = field_type(field);
        if kind != "signature" && kind != "initials" {
            continue;
        }
        *counters.entry(kind.clone()).or_default() += 1;
        if !is_for(field) {
            continue;
        }
        let value = value_of(field);
        let name = field_title(field)
            .unwrap_or_else(|| format!("{} {}", data.t(&format!("{kind}_field")), counters[&kind]));
        if let Some((_, names)) = grouped.iter_mut().find(|(v, _)| *v == value) {
            skip.push(
                field
                    .get("uuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            names.push_str(", ");
            names.push_str(&name);
        } else {
            grouped.push((value, name));
        }
    }

    let mut uploads = data
        .uploads
        .remove(&submitter.id)
        .ok_or_else(|| anyhow::anyhow!("uploads of submitter {}", submitter.id))?;
    let uploads = &mut uploads;
    let mut counters: HashMap<String, usize> = HashMap::new();
    for field in fields {
        let kind = field_type(field);
        *counters.entry(kind.clone()).or_default() += 1;
        let uuid = field
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_for(field)
            || kind == "heading"
            || kind == "strikethrough"
            || (!data.configs.with_audit_values && kind != "signature" && kind != "initials")
            || skip.iter().any(|s| s == uuid)
        {
            continue;
        }
        let value = value_of(field);
        if wrap(Some(&value)).iter().all(|v| blank(Some(v))) {
            continue;
        }
        let field_name = grouped
            .iter()
            .find(|(v, _)| *v == value)
            .map(|(_, n)| n.clone())
            .filter(|n| !n.trim().is_empty())
            .or_else(|| field_title(field))
            .unwrap_or_default();
        let value_text = ruby_to_s(&value);
        let attachment = uploads.find(&value_text).map(|(_, b)| b.clone());
        let mut kind_shown = kind.clone();
        if (kind == "image" || kind == "stamp")
            && attachment.as_ref().is_some_and(|b| {
                let content_type = b.content_type.as_deref().unwrap_or_default();
                !content_type.starts_with("image/")
                    || UNSUPPORTED_IMAGE_TYPES.contains(&content_type)
            })
        {
            "file".clone_into(&mut kind_shown);
        }

        let label = {
            let upper = ltr(&field_name)?.to_uppercase();
            if upper.trim().is_empty() {
                format!("{} {}", data.t(&format!("{kind}_field")), counters[&kind]).to_uppercase()
            } else {
                upper
            }
        };
        boxes.push(styles.text(
            vec![Piece::Regular(label)],
            6.0,
            1.3,
            Align::Left,
            padding(0.0, 0.0, 2.0, 0.0),
        ));

        if matches!(
            kind_shown.as_str(),
            "image" | "signature" | "initials" | "stamp" | "kba"
        ) && attachment.as_ref().is_some_and(|b| {
            b.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
        }) {
            let Some(image) = uploads.image(state, &value_text).await? else {
                continue;
            };
            let scale = (600.0 / f64::from(image.width())).min(600.0 / f64::from(image.height()));
            let image = if scale < 1.0 {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "image sizes"
                )]
                let (w, h) = (
                    ((f64::from(image.width()) * scale).round() as u32).max(1),
                    ((f64::from(image.height()) * scale).round() as u32).max(1),
                );
                image.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
            } else {
                image
            };
            let jpeg = kind == "image" && !image.color().has_alpha();
            let info = editor.add_image(&image, jpeg).map_err(pdf_error)?;
            let mut width = if kind == "initials" { 50.0 } else { 200.0 };
            let mut height = f64::from(info.height) * (width / f64::from(info.width));
            if height > MAX_IMAGE_HEIGHT {
                width *= MAX_IMAGE_HEIGHT / height;
                height = MAX_IMAGE_HEIGHT;
            }
            boxes.push(Content::Image(ImageBox {
                style: BoxStyle::default().margin(Quad::new(5.0, 0.0, 10.0, 0.0)),
                image: info.index,
                width,
                height,
            }));
        } else if matches!(kind_shown.as_str(), "file" | "payment" | "image") {
            if kind == "payment" {
                let currency = field
                    .get("preferences")
                    .and_then(|p| p.get("currency"))
                    .map(ruby_to_s)
                    .unwrap_or_default();
                let unit = match currency.as_str() {
                    "USD" => "$",
                    "EUR" => "€",
                    "GBP" => "£",
                    other => other,
                }
                .to_owned();
                let price = field
                    .get("preferences")
                    .and_then(|p| p.get("price"))
                    .map(ruby_to_s)
                    .unwrap_or_default();
                let price = crate::l10n::currency(state, &data.locale, &price, &unit);
                boxes.push(styles.text(
                    vec![Piece::Regular(format!(
                        "{}\n",
                        data.t_with("paid_price", &[("price", price)])
                    ))],
                    FONT_SIZE,
                    1.0,
                    Align::Left,
                    padding(0.0, 0.0, 10.0, 0.0),
                ));
            }
            let mut links = Vec::new();
            for uuid in wrap(Some(&value)) {
                let uuid = ruby_to_s(&uuid);
                let Some((_, blob)) = uploads.find(&uuid) else {
                    continue;
                };
                let mut blob = blob.clone();
                let uri = if data.configs.with_file_links {
                    crate::files::proxy_url(state, &mut blob, file_links_expire_at).await?
                } else {
                    format!("{}/e/{}", state.default_url(), data.form.submission.slug)
                };
                links.push(Piece::Link {
                    text: format!("{}\n", ltr(&blob.filename)?),
                    uri,
                });
            }
            boxes.push(styles.text(
                links,
                FONT_SIZE,
                1.0,
                Align::Left,
                padding(0.0, 0.0, 10.0, 0.0),
            ));
        } else if kind_shown == "checkbox" {
            boxes.push(styles.text(
                vec![Piece::Regular(titleize(&value_text))],
                FONT_SIZE,
                1.0,
                Align::Left,
                padding(0.0, 0.0, 10.0, 0.0),
            ));
        } else {
            let format = field
                .get("preferences")
                .and_then(|p| p.get("format"))
                .and_then(Value::as_str);
            let mut text = match kind.as_str() {
                "date" => crate::l10n::format_date_string(
                    state,
                    &value_text,
                    format,
                    &data.form.account.locale,
                    &data.timezone,
                ),
                "number" => crate::l10n::format_number(state, &value, format),
                _ => wrap(Some(&value))
                    .iter()
                    .map(ruby_to_s)
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            if let Some(mask) = field
                .get("preferences")
                .and_then(|p| p.get("mask"))
                .filter(|m| present(Some(m)))
            {
                text = crate::email_variables::mask_value(&text, mask);
            }
            let text = if text.is_empty() {
                "n/a".to_owned()
            } else {
                ltr(&text)?
            };
            boxes.push(styles.text(
                vec![Piece::Regular(text)],
                FONT_SIZE,
                1.0,
                Align::Left,
                padding(0.0, 0.0, 10.0, 0.0),
            ));
        }
    }
    Ok(())
}

/// The event log rows: the time and what happened.
async fn event_rows(
    data: &Data<'_>,
    styles: &Styles,
) -> Result<Vec<Vec<Option<Content>>>, AppError> {
    let mut events: Vec<&SubmissionEvent> = data.events.iter().collect();
    events.sort_by_key(|e| e.event_timestamp);
    let created_by = data.form.submission.created_by_user_id;
    let mut rows = Vec::new();
    for event in events {
        if matches!(
            event.event_type.as_str(),
            "bounce_email" | "complaint_email"
        ) {
            continue;
        }
        let Some(submitter) = event.submitter_id.and_then(|id| data.submitter(id)) else {
            continue;
        };
        let versions = data
            .versions
            .get(&submitter.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let active = versions
            .iter()
            .find(|v| v.created_at > event.event_timestamp);
        let event_data = &event.data.0;
        let kind = event.event_type.as_str();
        let name = if kind.contains("sms") || kind.contains("phone") {
            or(
                event_data.get("phone").and_then(Value::as_str),
                or(
                    active.and_then(|v| v.phone.as_deref()),
                    submitter.phone.as_deref(),
                ),
            )
        } else {
            [
                active.and_then(|v| v.name.as_deref()),
                active.and_then(|v| v.email.as_deref()),
                active.and_then(|v| v.phone.as_deref()),
                submitter.name.as_deref(),
                submitter.email.as_deref(),
                submitter.phone.as_deref(),
            ]
            .into_iter()
            .flatten()
            .next()
        };
        let submitter_name = ltr(&squish(name.unwrap_or_default()))?;
        let names = || ("submitter_name", submitter_name.clone());
        let invited = (kind == "invite_party")
            .then(|| {
                let uuid = event_data.get("uuid").and_then(Value::as_str)?;
                let invited = data.form.submitters.iter().find(|s| s.uuid == uuid)?;
                let role = data
                    .form
                    .template_submitters()
                    .iter()
                    .find(|s| s.get("uuid").and_then(Value::as_str) == Some(uuid))?
                    .get("name")
                    .map(ruby_to_s)?;
                let who = or(
                    invited.name.as_deref(),
                    or(invited.email.as_deref(), invited.phone.as_deref()),
                )
                .unwrap_or_default();
                Some(squish(&format!("{who} {role}")))
            })
            .flatten();
        let text = if kind == "complete_verification" {
            data.t_with(
                "submission_event_names.complete_verification_by_html",
                &[
                    (
                        "provider",
                        event_data.get("method").map(ruby_to_s).unwrap_or_default(),
                    ),
                    names(),
                ],
            )
        } else if let Some(invited) = invited {
            data.t_with(
                "submission_event_names.invite_party_by_html",
                &[("invited_submitter_name", ltr(&invited)?), names()],
            )
        } else if data.configs.with_audit_sender && (kind == "send_email" || kind == "send_sms") {
            let user = match created_by {
                Some(id) => users::find(&data.state.db, id).await?,
                None => None,
            };
            let user =
                user.ok_or_else(|| anyhow::anyhow!("the submission's creator is missing"))?;
            let full_name = [user.first_name.as_deref(), user.last_name.as_deref()]
                .into_iter()
                .flatten()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "{}\n<b>{}</b> {} {}",
                data.t_with(
                    &format!("submission_event_names.{kind}_to_html"),
                    &[names()]
                ),
                data.t("from"),
                ltr(&squish(&full_name))?,
                user.email
            )
        } else if kind == "delegate_form" {
            let from = event_data
                .get("old_email")
                .filter(|v| present(Some(v)))
                .map(ruby_to_s)
                .or_else(|| {
                    versions
                        .iter()
                        .rev()
                        .find(|v| v.created_at <= event.event_timestamp)
                        .and_then(|v| or(v.name.as_deref(), v.phone.as_deref()))
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            data.t_with(
                "submission_event_names.delegate_form_by_html",
                &[
                    ("from", ltr(&squish(&from))?),
                    (
                        "to",
                        event_data.get("email").map(ruby_to_s).unwrap_or_default(),
                    ),
                ],
            )
        } else if kind.contains("send_") {
            data.t_with(
                &format!("submission_event_names.{kind}_to_html"),
                &[names()],
            )
        } else {
            data.t_with(
                &format!("submission_event_names.{kind}_by_html"),
                &[names()],
            )
        };

        let mut pieces = Vec::new();
        for (index, row) in text.split('\n').enumerate() {
            if index > 0 {
                pieces.push(Piece::Regular("\n".to_owned()));
            }
            let (bold, rest) =
                split_bold(row).ok_or_else(|| anyhow::anyhow!("event text without <b>: {row}"))?;
            pieces.push(Piece::Bold(bold));
            pieces.push(Piece::Regular(rest));
        }
        let format = if data.configs.with_timestamp_seconds {
            "detailed"
        } else {
            "long"
        };
        rows.push(vec![
            Some(styles.plain(&data.time(event.event_timestamp, format))),
            Some(styles.text(pieces, FONT_SIZE, 1.0, Align::Left, BoxStyle::default())),
        ]);
    }
    Ok(rows)
}

/// `text.match(%r{<b>(.*?)</b>(.*)}).captures` on one line.
fn split_bold(text: &str) -> Option<(String, String)> {
    let start = text.find("<b>")?;
    let rest = &text[start + 3..];
    let end = rest.find("</b>")?;
    Some((rest[..end].to_owned(), rest[end + 4..].to_owned()))
}
