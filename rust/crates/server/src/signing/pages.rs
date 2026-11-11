//! `SubmitFormController#show`, `#completed` and `#success`, with the `submit_form/*`
//! state pages (awaiting, archived, expired, declined).

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use loco_db::signing;
use loco_domain::submit::{self as rules, blank, present, wrap};
use loco_storage::{Attachment, Blob};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    Access, Form, FormConfigs,
    actions::{redirect_to, request_origin},
    submit::default_value_for,
};
use crate::{
    api::rails_json,
    auth::{Auth, MaybeAuth},
    errors::AppError,
    l10n, locale,
    page::Page,
    session::Session,
    state::AppState,
};

/// `PdfUtils::US_LETTER_W`.
pub(super) const US_LETTER_W: f64 = 72.0 * 8.5;
/// `Submissions::GenerateResultAttachments::FONT_SIZE`.
const FONT_SIZE: i64 = 11;
/// `Templates::ProcessDocument::US_LETTER_SIZE`.
pub(super) const US_LETTER_SIZE: (i64, i64) = (1400, 1812);

#[derive(Deserialize)]
pub struct ShowQuery {
    lang: Option<String>,
    signature: Option<String>,
}

/// `Float#to_s`: `25.0`, `17.973856209150327`, `1.0e-05`.
#[must_use]
pub fn ruby_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        let s = format!("{f:e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') {
            mantissa.to_owned()
        } else {
            format!("{mantissa}.0")
        };
        let exp: i32 = exponent.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    if f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

pub(super) fn float(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `Templates::COLOR_REGEXP`.
fn valid_color(color: &str) -> bool {
    if let Some(hex) = color.strip_prefix('#') {
        (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        !color.is_empty() && color.chars().all(|c| c.is_ascii_alphabetic())
    }
}

/// The request locale of the form pages (`with_browser_locale`).
pub(super) fn form_locale(
    state: &AppState,
    auth: Option<&Auth>,
    lang: Option<&str>,
    headers: &HeaderMap,
) -> String {
    let current = locale::app_locale(state, auth, None);
    locale::browser_locale(&current, lang, headers)
}

fn title(form: &Form) -> String {
    form.submission
        .name
        .clone()
        .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default()
}

/// A state page (`submit_form/<name>.html.erb`) with the shared header.
fn state_page(
    state: &AppState,
    session: &Session,
    locale: &str,
    form: &Form,
    template: &str,
    extra: Value,
) -> Result<Response, AppError> {
    Page::new(state, session, locale)
        .with(json!({
            "name": title(form),
            "account_name": form.account.name,
            "robots_noindex": true,
        }))
        .with(extra)
        .render(template)
}

/// `maybe_render_locked_page`.
fn locked_page(
    state: &AppState,
    session: &Session,
    locale: &str,
    form: &Form,
) -> Option<Result<Response, AppError>> {
    if form.template_archived()
        || form.submission.archived_at.is_some()
        || form.account.archived_at.is_some()
    {
        return Some(state_page(
            state,
            session,
            locale,
            form,
            "submit_form/archived.html",
            json!({}),
        ));
    }
    if form.expired() {
        let time = form
            .submission
            .expire_at
            .map(|at| l10n::local_date(state, locale, at, &form.account.timezone, "long"));
        return Some(state_page(
            state,
            session,
            locale,
            form,
            "submit_form/expired.html",
            json!({ "time": time }),
        ));
    }
    if let Some(at) = form.submitter.declined_at {
        let time = l10n::local_date(state, locale, at, &form.account.timezone, "long");
        return Some(state_page(
            state,
            session,
            locale,
            form,
            "submit_form/declined.html",
            json!({ "time": time }),
        ));
    }
    None
}

/// `GET /s/:slug`.
pub async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<ShowQuery>,
) -> Result<Response, AppError> {
    let locale = form_locale(&state, auth.as_ref(), query.lang.as_deref(), &headers);
    let Some(form) = Form::load(&state, &slug).await? else {
        return delegated_or_not_found(&state, &headers, &slug).await;
    };
    if let Some(page) = locked_page(&state, &session, &locale, &form) {
        return page;
    }
    let empty = Value::Object(Map::new());
    let access = Access {
        state: &state,
        headers: &headers,
        params: &empty,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if !access.pass_link_2fa(&form) {
        let template_slug = form
            .template
            .as_ref()
            .map(|t| t.slug.clone())
            .unwrap_or_default();
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/d/{template_slug}"),
        ));
    }
    if !access.pass_email_2fa(&form) {
        return Page::new(&state, &session, &locale)
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .with(json!({
                "name": title(&form),
                "message": "Email two-factor verification is not supported yet",
            }))
            .render("submit_form/unsupported.html");
    }
    if form.submitter.completed_at.is_some() || form.submission.completed_at.is_some() {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/s/{}/completed", form.submitter.slug),
        ));
    }

    let configs = FormConfigs::load(&state, form.account.id).await?;
    let preserved = form
        .template_preference("submitters_order")
        .and_then(Value::as_str)
        == Some("preserved");
    if (configs.enforce_signing_order || preserved) && !form.current_submitter_order() {
        return state_page(
            &state,
            &session,
            &locale,
            &form,
            "submit_form/awaiting.html",
            json!({}),
        );
    }

    let Some(documents) = schema_documents(&state, &form).await? else {
        return Page::new(&state, &session, &locale)
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .with(json!({
                "name": title(&form),
                "message": "Templates with dynamic documents are not supported yet",
            }))
            .render("submit_form/unsupported.html");
    };
    let origin = request_origin(&state, &headers);
    let view = ShowView::build(&state, &form, &configs, &documents, &origin, &locale).await?;

    let mut current_user_data = Value::Null;
    let mut prefill_initials = Value::Null;
    let mut prefill_signature = Value::Null;
    if let Some(auth) = auth
        .as_ref()
        .filter(|a| form.submitter.email.as_deref() == Some(a.user.email.as_str()))
    {
        current_user_data = json!({
            "first_name": auth.user.first_name,
            "last_name": auth.user.last_name,
            "full_name": auth.user.full_name(),
        });
        if let Some(data) =
            user_attachment(&state, &form, &origin, auth.user.id, "initials").await?
        {
            prefill_initials = data;
        }
    }
    if configs.prefill_signature
        && let Some(auth) = auth.as_ref()
        && let Some(data) =
            user_attachment(&state, &form, &origin, auth.user.id, "signature").await?
    {
        prefill_signature = data;
    }

    let form_data = submission_form_data(
        &state,
        &form,
        &configs,
        &view,
        &locale,
        query.signature.as_deref(),
        [prefill_signature, prefill_initials, current_user_data],
    )
    .await?;

    let description = state.views.t(
        &form.account.locale,
        "account_name_has_invited_you_to_fill_and_sign_documents_online_effortlessly_with_a_secure_fast_and_user_friendly_digital_document_signing_solution",
        &[("account_name", form.account.name.clone())],
    );
    let viewer = form.viewer();
    Page::new(&state, &session, &locale)
        .with(json!({
            "html_title": format!("{} | Loco Lokal", title(&form)),
            "html_description": description,
            "robots_noindex": true,
            "form_title": title(&form),
            "with_decline": configs.with_decline && !viewer,
            "with_delegate": configs.with_delegate && !viewer,
            "with_partial_download": configs.with_partial_download,
            "decline_modal_id": uuid::Uuid::new_v4().to_string(),
            "delegate_modal_id": uuid::Uuid::new_v4().to_string(),
            "slug": form.submitter.slug,
            "documents": view.documents,
            "policy_links_html": configs
                .policy_links
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(crate::markdown::to_html),
            "form": form_data,
            "nonce": csp_nonce(),
        }))
        .render("submit_form/show.html")
}

/// `content_security_policy_nonce_generator`: `SecureRandom.base64(16)`.
pub(super) fn csp_nonce() -> String {
    use base64::Engine as _;
    let mut bytes = [0_u8; 16];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `maybe_redirect_delegated`: a slug that belonged to a delegated submitter version.
async fn delegated_or_not_found(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
) -> Result<Response, AppError> {
    let Some(submitter_id) = signing::submitter_version_submitter_id(&state.db, slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if signing::has_event(&state.db, submitter_id, &["delegate_form"]).await? {
        return Ok(redirect_to(state, headers, &format!("/s/{slug}/delegated")));
    }
    Err(AppError::Status(StatusCode::NOT_FOUND))
}

/// `Submitters.build_prefill_attachment_data` for the user's saved signature or initials
/// (`UserConfigs.load_signature` / `load_initials`).
async fn user_attachment(
    state: &AppState,
    form: &Form,
    origin: &str,
    user_id: i64,
    key: &str,
) -> Result<Option<Value>, AppError> {
    let Some(uuid) = signing::user_config(&state.db, user_id, key)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let attachments =
        Attachment::with_blobs_for_records(&state.db, "User", &[user_id], key).await?;
    let Some((attachment, mut blob)) = attachments.into_iter().find(|(a, _)| a.uuid == uuid) else {
        return Ok(None);
    };
    blob.ensure_uuid(&state.db).await?;
    let token = state.secrets.signed_id_verifier.generate(
        &json!([attachment.uuid, form.submitter.slug]),
        Some("prefill_attachment"),
        None,
    );
    Ok(Some(
        json!({ "token": token, "url": crate::disk::blob_url(state, origin, &blob) }),
    ))
}

/// A document of the form: its attachment, blob and page previews.
pub struct Document {
    pub attachment: Attachment,
    pub blob: Blob,
    pub previews: Vec<(Attachment, Blob)>,
}

/// `submission.schema_documents` with previews; `None` for dynamic documents.
pub async fn schema_documents(
    state: &AppState,
    form: &Form,
) -> Result<Option<Vec<Document>>, AppError> {
    if form.schema().iter().any(|s| present(s.get("dynamic"))) {
        return Ok(None);
    }
    let (record_type, record_id) = match &form.template {
        Some(template)
            if form
                .submission
                .variables_schema
                .as_ref()
                .is_none_or(|v| blank(Some(v))) =>
        {
            ("Template", template.id)
        }
        Some(_) | None => ("Submission", form.submission.id),
    };
    let uuids: Vec<&str> = form
        .schema()
        .iter()
        .filter_map(|s| str_of(s, "attachment_uuid"))
        .collect();
    let mut documents = Vec::new();
    for (attachment, mut blob) in
        Attachment::with_blobs_for_records(&state.db, record_type, &[record_id], "documents")
            .await?
    {
        if record_type == "Template" && !uuids.contains(&attachment.uuid.as_str()) {
            continue;
        }
        blob.ensure_uuid(&state.db).await?;
        documents.push(Document {
            attachment,
            blob,
            previews: Vec::new(),
        });
    }
    let ids: Vec<i64> = documents.iter().map(|d| d.attachment.id).collect();
    let mut previews: HashMap<i64, Vec<(Attachment, Blob)>> = HashMap::new();
    if !ids.is_empty() {
        for (a, mut b) in Attachment::with_blobs_for_records(
            &state.db,
            "ActiveStorage::Attachment",
            &ids,
            "preview_images",
        )
        .await?
        {
            b.ensure_uuid(&state.db).await?;
            previews.entry(a.record_id).or_default().push((a, b));
        }
    }
    for document in &mut documents {
        document.previews = previews.remove(&document.attachment.id).unwrap_or_default();
    }
    Ok(Some(documents))
}

/// What `submit_form/show` renders per document and page.
pub struct ShowView {
    pub documents: Vec<Value>,
    pub schema: Vec<Value>,
    pub attachments: HashMap<String, (Attachment, Blob)>,
    pub origin: String,
}

impl ShowView {
    async fn build(
        state: &AppState,
        form: &Form,
        configs: &FormConfigs,
        documents: &[Document],
        origin: &str,
        locale: &str,
    ) -> Result<Self, AppError> {
        let option_word = form.option_word(state);
        let values = form.all_values();
        let schema = rules::filtered_conditions_schema(
            form.schema(),
            form.fields(),
            &values,
            Some(&form.submitter.uuid),
            &option_word,
        );
        let fields_index = rules::field_areas_index(form.fields());
        let submitter_ids: Vec<i64> = form.submitters.iter().map(|s| s.id).collect();
        let mut attachments = HashMap::new();
        for (a, mut b) in Attachment::with_blobs_for_records(
            &state.db,
            "Submitter",
            &submitter_ids,
            "attachments",
        )
        .await?
        {
            b.ensure_uuid(&state.db).await?;
            attachments.insert(a.uuid.clone(), (a, b));
        }
        let mut view = Self {
            documents: Vec::new(),
            schema: schema.clone(),
            attachments,
            origin: origin.to_owned(),
        };

        let font_scale = 1000.0 / US_LETTER_W;
        let mut rendered = Vec::new();
        for item in &schema {
            let Some(document) = documents
                .iter()
                .find(|d| Some(d.attachment.uuid.as_str()) == str_of(item, "attachment_uuid"))
            else {
                continue;
            };
            let metadata = document.blob.metadata_json();
            let annots = metadata
                .pointer("/pdf/annotations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let preview_index: HashMap<i64, &(Attachment, Blob)> = document
                .previews
                .iter()
                .map(|p| {
                    (
                        crate::api::serialize::filename_base(&p.1.filename)
                            .parse()
                            .unwrap_or(-1),
                        p,
                    )
                })
                .collect();
            let lazy_size = document.previews.last().map_or(US_LETTER_SIZE, |(_, b)| {
                let m = b.metadata_json();
                (
                    m.get("width")
                        .and_then(Value::as_i64)
                        .unwrap_or(US_LETTER_SIZE.0),
                    m.get("height")
                        .and_then(Value::as_i64)
                        .unwrap_or(US_LETTER_SIZE.1),
                )
            });
            let pages = metadata
                .pointer("/pdf/number_of_pages")
                .and_then(Value::as_i64)
                .unwrap_or(i64::try_from(document.previews.len()).unwrap_or_default());
            let signed_key = state.secrets.signed_id_verifier.generate(
                &json!([document.attachment.id, document.attachment.uuid]),
                Some("attachment"),
                Some(Utc::now() + Duration::hours(6)),
            );
            let mut page_views = Vec::new();
            for index in 0..pages {
                let (url, width, height) = match preview_index.get(&index) {
                    Some((_, blob)) => {
                        let m = blob.metadata_json();
                        (
                            crate::disk::blob_url(state, origin, blob),
                            m.get("width").cloned().unwrap_or(Value::Null),
                            m.get("height").cloned().unwrap_or(Value::Null),
                        )
                    }
                    None => (
                        format!("/preview/{signed_key}/{index}.jpg"),
                        json!(lazy_size.0),
                        json!(lazy_size.1),
                    ),
                };
                let page_width = float(Some(&width));
                let page_height = float(Some(&height));
                let page_annots: Vec<Value> = annots
                    .iter()
                    .filter(|a| a.get("page").and_then(Value::as_i64) == Some(index))
                    .map(|a| {
                        json!({
                            "value": a.get("value"),
                            "w": ruby_float(float(a.get("w")) * 100.0),
                            "h": ruby_float(float(a.get("h")) * 100.0),
                            "x": ruby_float(float(a.get("x")) * 100.0),
                            "y": ruby_float(float(a.get("y")) * 100.0),
                        })
                    })
                    .collect();
                let mut field_values = Vec::new();
                for (area, field) in fields_index
                    .get(&document.attachment.uuid)
                    .and_then(|pages| pages.get(&index))
                    .into_iter()
                    .flatten()
                {
                    if let Some(v) = view.field_value(
                        state,
                        form,
                        configs,
                        &values,
                        area,
                        field,
                        page_width,
                        page_height,
                        font_scale,
                    ) {
                        field_values.push(v);
                    }
                }
                page_views.push(json!({
                    "index": index,
                    "width": width,
                    "height": height,
                    "url": url,
                    "alt": format!(
                        "{} - {} {}",
                        str_of(item, "name").unwrap_or_default(),
                        state.views.t(locale, "page", &[]),
                        index + 1
                    ),
                    "annots": page_annots,
                    "values": field_values,
                }));
            }
            rendered.push(json!({ "uuid": document.attachment.uuid, "pages": page_views }));
        }
        view.documents = rendered;
        Ok(view)
    }

    /// One `submissions/_value` partial, or `None` when the show loop skips the field.
    #[allow(clippy::too_many_arguments, reason = "the partial's locals")]
    fn field_value(
        &self,
        state: &AppState,
        form: &Form,
        configs: &FormConfigs,
        values: &Map<String, Value>,
        area: &Value,
        field: &Value,
        page_width: f64,
        page_height: f64,
        font_scale: f64,
    ) -> Option<Value> {
        let uuid = str_of(field, "uuid").unwrap_or_default();
        let owner_uuid = str_of(field, "submitter_uuid").unwrap_or_default();
        let own = owner_uuid == form.submitter.uuid;
        let readonly = field.get("readonly") == Some(&Value::Bool(true));
        let owner = form.submitters.iter().find(|s| s.uuid == owner_uuid);
        let value = values
            .get(uuid)
            .filter(|v| present(Some(v)))
            .cloned()
            .or_else(|| {
                let default = field.get("default_value").filter(|v| present(Some(v)))?;
                if !readonly {
                    return None;
                }
                default_value_for(state, form, owner?, default, field, false)
            })?;
        if blank(Some(&value)) {
            return None;
        }
        if !readonly && own {
            return None;
        }
        if value.as_str() == Some("{{date}}") && !own {
            return None;
        }
        let has_conditions = present(field.get("conditions"));
        if has_conditions && blank(values.get(uuid)) && !own {
            return None;
        }
        if has_conditions && own {
            return None;
        }
        if present(field.pointer("/preferences/formula")) && own {
            return None;
        }
        let value = match field
            .pointer("/preferences/mask")
            .filter(|m| present(Some(m)))
        {
            Some(mask) => Value::String(crate::email_variables::mask_value(
                &crate::api::create::values::ruby_to_s(&value),
                mask,
            )),
            None => value,
        };
        let owner = owner?;
        let locals = ValueLocals {
            locale: &form.account.locale,
            timezone: &form.account.timezone,
            with_signature_id: configs.with_signature_id,
            with_submitter_timezone: configs.with_submitter_timezone,
            with_timestamp_seconds: configs.with_timestamp_seconds,
            with_signature_id_reason: configs.with_signature_id_reason,
            with_signature_id_completed_at: configs.with_signature_id_completed_at,
            page_width,
            page_height,
            font_scale,
        };
        Some(value_partial(
            state,
            &self.attachments,
            &self.origin,
            &locals,
            area,
            field,
            &value,
            Some(owner),
        ))
    }
}

/// The locals of the `submissions/_value` partial besides the field, its area and value.
#[allow(clippy::struct_excessive_bools, reason = "the partial's locals")]
pub struct ValueLocals<'a> {
    pub locale: &'a str,
    pub timezone: &'a str,
    pub with_signature_id: bool,
    pub with_submitter_timezone: bool,
    pub with_timestamp_seconds: bool,
    pub with_signature_id_reason: bool,
    pub with_signature_id_completed_at: bool,
    pub page_width: f64,
    pub page_height: f64,
    pub font_scale: f64,
}

/// The view model of one `submissions/_value` partial (`v` in the template).
#[allow(
    clippy::too_many_arguments,
    clippy::implicit_hasher,
    reason = "the partial's locals"
)]
#[must_use]
pub fn value_partial(
    state: &AppState,
    attachments: &HashMap<String, (Attachment, Blob)>,
    origin: &str,
    opts: &ValueLocals<'_>,
    area: &Value,
    field: &Value,
    value: &Value,
    submitter: Option<&loco_db::submissions::Submitter>,
) -> Value {
    let ValueLocals {
        page_width,
        page_height,
        font_scale,
        ..
    } = *opts;
    let attachment_url = |uuid: &str| {
        attachments
            .get(uuid)
            .map(|(_, blob)| crate::disk::blob_url(state, origin, blob))
    };
    let pref = |key: &str| {
        field
            .pointer(&format!("/preferences/{key}"))
            .and_then(Value::as_str)
    };
    let align = pref("align");
    let valign = pref("valign");
    let color = pref("color").filter(|c| valid_color(c));
    let bg_color = pref("background").filter(|c| valid_color(c));
    let font = pref("font");
    let font_type = pref("font_type");
    let font_size = field
        .pointer("/preferences/font_size")
        .filter(|v| present(Some(v)))
        .map_or(FONT_SIZE, |v| match v {
            #[allow(clippy::cast_possible_truncation, reason = "Float#to_i")]
            Value::Number(n) => n
                .as_i64()
                .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
            Value::String(s) => s.trim().parse().unwrap_or(0),
            _ => 0,
        });
    #[allow(clippy::cast_precision_loss, reason = "font sizes are small")]
    let font_size_px = font_size as f64 * font_scale;
    let class = format!(
        "flex absolute {} {} {} {} {}",
        if font == Some("Courier") {
            "font-courier"
        } else {
            ""
        },
        if font == Some("Times") {
            "font-times"
        } else {
            ""
        },
        if matches!(font_type, Some("bold" | "bold_italic")) {
            "font-bold"
        } else {
            ""
        },
        if matches!(font_type, Some("italic" | "bold_italic")) {
            "italic"
        } else {
            ""
        },
        match align {
            Some("right") => "text-right",
            Some("center") => "text-center",
            _ => "",
        },
    );
    let fs1 = format!(
        "clamp(1pt, {}vw, {}px)",
        ruby_float(font_size_px / 10.0),
        ruby_float(font_size_px)
    );
    let fs2 = format!("{}cqmin", ruby_float(font_size_px / 10.0));
    let style = format!(
        "{}{}width: {}%; height: {}%; left: {}%; top: {}%; font-size: {fs1}; line-height: calc({fs1} * 1.3); font-size: {fs2}; line-height: calc({fs2} * 1.3)",
        color.map(|c| format!("color: {c}; ")).unwrap_or_default(),
        bg_color
            .map(|c| format!("background: {c}; "))
            .unwrap_or_default(),
        ruby_float(float(area.get("w")) * 100.0),
        ruby_float(float(area.get("h")) * 100.0),
        ruby_float(float(area.get("x")) * 100.0),
        ruby_float(float(area.get("y")) * 100.0),
    );
    let valign_class = match valign {
        Some("top") => "items-start",
        Some("bottom") => "items-end",
        _ => "items-center",
    };
    let field_type = str_of(field, "type").unwrap_or_default();
    let alt = str_of(field, "name")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| str_of(field, "title").filter(|s| !s.trim().is_empty()))
        .unwrap_or(field_type)
        .to_owned();
    let value_str = value.as_str().unwrap_or_default();
    let attachment = attachments.get(value_str);
    let is_image = attachment.is_some_and(|(_, b)| {
        b.content_type
            .as_deref()
            .is_some_and(|c| c.starts_with("image/"))
    });
    let locale = opts.locale;
    let mut out = json!({
        "class": class,
        "style": style,
        "valign_class": valign_class,
        "alt": alt,
    });
    let w = float(area.get("w"));
    let h = float(area.get("h"));
    let check_class = if w > h {
        "!w-auto !h-full"
    } else {
        "!w-full !h-auto"
    };

    match field_type {
        "signature" => {
            let is_narrow = h > 0.0 && (w * page_width) / (h * page_height) > 4.5;
            let reason_uuid = pref("reason_field_uuid").filter(|s| !s.trim().is_empty());
            let show_id = opts.with_signature_id || reason_uuid.is_some();
            let mut signature = json!({
                "kind": "signature",
                "row": is_narrow && show_id,
                "narrow": is_narrow,
                "src": attachment_url(value_str),
            });
            if show_id && let Some((attachment, _)) = attachment {
                let reason_value = reason_uuid
                    .and_then(|u| submitter?.values.get(u))
                    .filter(|v| present(Some(v)))
                    .map(crate::api::create::values::ruby_to_s);
                let timezone = submitter
                    .and_then(|s| s.timezone.clone())
                    .filter(|_| opts.with_submitter_timezone)
                    .unwrap_or_else(|| opts.timezone.to_owned());
                let format = if opts.with_timestamp_seconds {
                    "detailed"
                } else {
                    "long"
                };
                let timestamp = if opts.with_signature_id_completed_at {
                    submitter
                        .and_then(|s| s.completed_at)
                        .unwrap_or(attachment.created_at)
                } else {
                    attachment.created_at
                };
                signature["id"] = json!({
                    "uuid": attachment.uuid,
                    "with_reason": opts.with_signature_id_reason,
                    "reason_value": reason_value,
                    "name": submitter.and_then(|s| s.name.clone()),
                    "email": submitter.and_then(|s| s.email.clone()),
                    "time": l10n::time(state, locale, timestamp, &timezone, format),
                    "zone": l10n::timezone_abbr(&timezone, timestamp),
                });
            }
            merge(&mut out, signature);
        }
        "image" | "initials" | "stamp" | "kba" if is_image => {
            merge(
                &mut out,
                json!({ "kind": "image", "src": attachment_url(value_str) }),
            );
        }
        "file" | "payment" | "image" => {
            let files: Vec<Value> = wrap(Some(value))
                .iter()
                .filter_map(|v| {
                    let (_, blob) = attachments.get(v.as_str()?)?;
                    Some(json!({
                        "url": crate::disk::blob_url(state, origin, blob),
                        "filename": blob.filename,
                    }))
                })
                .collect();
            merge(&mut out, json!({ "kind": "files", "files": files }));
        }
        "checkbox" => merge(
            &mut out,
            json!({ "kind": "check", "check_class": check_class }),
        ),
        "multiple" | "radio" if area.get("option_uuid").is_some_and(|v| !v.is_null()) => {
            let options = field.get("options").and_then(Value::as_array);
            let checked = options
                .and_then(|options| {
                    let option = options
                        .iter()
                        .find(|o| o.get("uuid") == area.get("option_uuid"))?;
                    let name = rules::option_name(field, option, "Option");
                    Some(
                        wrap(Some(value))
                            .iter()
                            .any(|v| v.as_str() == Some(name.as_str())),
                    )
                })
                .unwrap_or(false);
            merge(
                &mut out,
                json!({ "kind": "option", "checked": checked, "check_class": check_class }),
            );
        }
        "cells" if float(area.get("cell_w")) > 0.0 => {
            let cell_w = float(area.get("cell_w"));
            let chars: Vec<char> = value_str.chars().collect();
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "cell counts"
            )]
            let count = (w / cell_w).ceil() as usize;
            let cells: Vec<String> = (0..=count)
                .filter_map(|i| chars.get(i).map(char::to_string))
                .collect();
            merge(
                &mut out,
                json!({
                    "kind": "cells",
                    "cells": cells,
                    "cell_width": ruby_float(cell_w / w * 100.0),
                    "justify_end": align == Some("right"),
                }),
            );
        }
        "date" => {
            let tz = submitter
                .and_then(|s| s.timezone.clone())
                .filter(|t| opts.with_submitter_timezone && !t.trim().is_empty())
                .unwrap_or_else(|| opts.timezone.to_owned());
            let text = if value_str == "{{date}}" {
                l10n::format_date_preview(state, pref("format"), locale, &tz)
            } else {
                l10n::format_date_string(
                    state,
                    &crate::api::create::values::ruby_to_s(value),
                    pref("format"),
                    locale,
                    &tz,
                )
            };
            merge(&mut out, json!({ "kind": "date", "text": text }));
        }
        "number" => merge(
            &mut out,
            json!({ "kind": "number", "text": l10n::format_number(state, value, pref("format")) }),
        ),
        "strikethrough" => {
            let thin = ((1000.0 / page_width) * page_height) * h < 40.0;
            merge(
                &mut out,
                json!({ "kind": "strikethrough", "thin": thin, "color": color.unwrap_or("red") }),
            );
        }
        _ => {
            let text = wrap(Some(value))
                .iter()
                .map(crate::api::create::values::ruby_to_s)
                .collect::<Vec<_>>()
                .join(", ");
            merge(&mut out, json!({ "kind": "text", "text": text }));
        }
    }
    out
}

fn merge(target: &mut Value, extra: Value) {
    if let (Value::Object(target), Value::Object(extra)) = (target, extra) {
        target.extend(extra);
    }
}

/// The data attributes of `<submission-form>` (`submit_form/_submission_form`).
async fn submission_form_data(
    state: &AppState,
    form: &Form,
    configs: &FormConfigs,
    view: &ShowView,
    locale: &str,
    signature_text: Option<&str>,
    [prefill_signature, prefill_initials, current_user]: [Value; 3],
) -> Result<Value, AppError> {
    let option_word = form.option_word(state);
    let values = form.all_values();
    let fields = rules::filtered_conditions_fields(
        form.fields(),
        &values,
        &form.submitter.uuid,
        &option_word,
    );
    let invites = |key: &str| -> Vec<Value> {
        form.template_submitters()
            .iter()
            .filter(|s| str_of(s, key) == Some(form.submitter.uuid.as_str()))
            .filter(|s| {
                !form
                    .submitters
                    .iter()
                    .any(|e| Some(e.uuid.as_str()) == str_of(s, "uuid"))
            })
            .cloned()
            .collect()
    };
    let mut own_attachments: Vec<(&Attachment, &Blob)> = view
        .attachments
        .values()
        .filter(|(a, _)| a.record_id == form.submitter.id)
        .map(|(a, b)| (a, b))
        .collect();
    own_attachments.sort_by_key(|(a, _)| a.id);
    let attachments: Vec<Value> = own_attachments
        .iter()
        .map(|(a, b)| super::actions::attachment_json(state, &view.origin, a, b))
        .collect();

    let completed_message = match configs.completed_message.as_object() {
        Some(map) if map.values().any(|v| present(Some(v))) => Value::Object(
            map.iter()
                .filter(|(_, v)| present(Some(v)))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => form
            .template_preference("completed_message")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!({})),
    };
    let redirect_url = form
        .submitter
        .preferences
        .get("completed_redirect_url")
        .filter(|v| present(Some(v)))
        .or_else(|| form.template_preference("completed_redirect_url"))
        .map(crate::api::create::values::ruby_to_s);
    let go_to_last = match form.submitter.preferences.get("go_to_last") {
        Some(v)
            if form
                .submitter
                .preferences
                .as_object()
                .is_some_and(|p| p.contains_key("go_to_last")) =>
        {
            crate::api::create::values::ruby_to_s(v)
        }
        _ => form.submitter.opened_at.is_some().to_string(),
    };
    // `to_json(only: %i[uuid slug name phone email])` keeps the `only:` order.
    let submitter_json = json!({
        "uuid": form.submitter.uuid,
        "slug": form.submitter.slug,
        "name": form.submitter.name,
        "phone": form.submitter.phone,
        "email": form.submitter.email,
    });
    let view_token = state.secrets.signed_id_verifier.generate(
        &json!(form.submitter.slug),
        Some("view_form"),
        Some(Utc::now() + Duration::minutes(1)),
    );
    let json_or_empty = |v: &Value| {
        if v.is_null() {
            String::new()
        } else {
            rails_json(v)
        }
    };
    // Every attribute as ERB prints it: `nil` is empty, `true` is "true".
    let attrs = json!({
        "schema": rails_json(&Value::Array(view.schema.clone())),
        "reuse_signature": configs.reuse_signature,
        "require_signing_reason": configs.require_signing_reason,
        "with_signature_id": configs.with_signature_id,
        "with_field_labels": configs.with_field_labels,
        "with_confetti": configs.with_confetti,
        "completed_redirect_url": redirect_url,
        "completed_message": rails_json(&completed_message),
        "completed_button": rails_json(&configs.completed_button),
        "go_to_last": go_to_last,
        "submitter": rails_json(&submitter_json),
        "can_send_email": crate::mail::can_send_emails(state).await?,
        "optional_invite_submitters": rails_json(&Value::Array(invites("optional_invite_by_uuid"))),
        "invite_submitters": rails_json(&Value::Array(invites("invite_by_uuid"))),
        "attachments": rails_json(&Value::Array(attachments)),
        "fields": rails_json(&Value::Array(fields)),
        "values": rails_json(&form.submitter.values),
        "with_typed_signature": configs.with_typed_signature,
        "signature_text": signature_text,
        "prefill_signature": json_or_empty(&prefill_signature),
        "prefill_initials": json_or_empty(&prefill_initials),
        "current_user": json_or_empty(&current_user),
        "remember_signature": configs.prefill_signature,
        "view_token": view_token,
        "language": locale.split('-').next().unwrap_or(locale),
    });
    let attrs = attrs
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| {
            let text = match v {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), Value::String(text))
        })
        .collect();
    Ok(Value::Object(attrs))
}

/// `GET /s/:slug/completed`.
pub async fn completed(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<ShowQuery>,
) -> Result<Response, AppError> {
    let locale = form_locale(&state, auth.as_ref(), query.lang.as_deref(), &headers);
    let Some(form) = Form::load(&state, &slug).await? else {
        return delegated_or_not_found(&state, &headers, &slug).await;
    };
    if form.account.archived_at.is_some() {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    }
    let empty = Value::Object(Map::new());
    let access = Access {
        state: &state,
        headers: &headers,
        params: &empty,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if !access.authorized(&form) {
        return Ok(redirect_to(&state, &headers, &format!("/s/{slug}")));
    }
    let Some(completed_at) = form.submitter.completed_at.or(form.submission.completed_at) else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "the form is not completed"
        )));
    };
    let with_signature_fields = form.fields().iter().any(|f| {
        str_of(f, "submitter_uuid") == Some(form.submitter.uuid.as_str())
            && matches!(str_of(f, "type"), Some("signature" | "initials"))
    });
    let time = l10n::local_date(
        &state,
        &locale,
        completed_at,
        &form.account.timezone,
        "long",
    );
    let can_email = crate::mail::can_send_emails(&state).await?
        && form
            .submitter
            .email
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty());
    let undefined = super::start_form::undefined_submitters(form.template_submitters());
    let can_resubmit = undefined.len() == 1
        && undefined[0].get("uuid").and_then(Value::as_str) == Some(form.submitter.uuid.as_str())
        && form
            .template
            .as_ref()
            .is_some_and(|t| t.archived_at.is_none())
        && super::start_form::can_resubmit(&state, &form).await?;
    let recent = completed_at > Utc::now().naive_utc() - Duration::minutes(30);
    let owner = auth
        .as_ref()
        .is_some_and(|a| a.account.id == form.submitter.account_id);
    Page::new(&state, &session, &locale)
        .auth(auth.as_ref())
        .with(json!({
            "name": title(&form),
            "account_name": form.account.name,
            "robots_noindex": true,
            "signed_text": state.views.t(
                &locale,
                if with_signature_fields { "signed_on_time" } else { "completed_on_time" },
                &[("time", time)],
            ),
            "can_email": can_email,
            "single_party": undefined.len() == 1,
            "can_resubmit": can_resubmit,
            "can_download": recent || owner,
            "slug": form.submitter.slug,
        }))
        .render("submit_form/completed.html")
}

/// `GET /success`.
pub async fn success(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<ShowQuery>,
) -> Result<Response, AppError> {
    let locale = form_locale(&state, auth.as_ref(), query.lang.as_deref(), &headers);
    Ok(Page::new(&state, &session, &locale)
        .with(json!({ "robots_noindex": true }))
        .render("submit_form/success.html")?
        .into_response())
}

/// `GET /s/:slug/delegated` (`SubmitFormController#delegated`): the old slug of a
/// delegated form.
pub async fn delegated(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<ShowQuery>,
) -> Result<Response, AppError> {
    let locale = form_locale(&state, auth.as_ref(), query.lang.as_deref(), &headers);
    let Some(submitter_id) = signing::submitter_version_submitter_id(&state.db, &slug).await?
    else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let submitter = loco_db::submissions::find_submitter(&state.db, submitter_id)
        .await?
        .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
    let form = Form::for_submitter(&state, submitter).await?;
    if let Some(page) = locked_page(&state, &session, &locale, &form) {
        return page;
    }
    let delegated_at =
        signing::latest_event_timestamp(&state.db, form.submitter.id, "delegate_form")
            .await?
            .ok_or(AppError::Status(StatusCode::NOT_FOUND))?;
    let time = l10n::local_date(
        &state,
        &locale,
        delegated_at,
        &form.account.timezone,
        "long",
    );
    state_page(
        &state,
        &session,
        &locale,
        &form,
        "submit_form/delegated.html",
        json!({ "time": time }),
    )
}
