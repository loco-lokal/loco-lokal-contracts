//! The submission preview: `SubmissionsPreviewController` (`/e/:slug`, the target of
//! documents copy links, and `/e/:slug/completed`), `SubmissionsPreviewDownloadController`
//! (`/e/:slug/download`) and `SubmitFormDrawSignatureController` (`/p/:slug`, drawing a
//! signature on a phone).
//!
//! The page is `submissions/show` with `is_preview: true`, including the parts a signed-in
//! user of the account sees.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Months, Utc};
use loco_db::{
    signing,
    submissions::{self, Submitter},
};
use loco_domain::submit::{self as rules, blank, present, wrap};
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};

use super::{
    Access, Form, FormConfigs,
    actions::{redirect_to, request_origin},
    downloads,
    pages::{self, Document, US_LETTER_SIZE, US_LETTER_W, ValueLocals, float, ruby_float},
    submit::default_value_for,
};
use crate::{
    api::{Params, create::values::ruby_to_s},
    auth::{Auth, MaybeAuth},
    email_variables::mask_value,
    errors::AppError,
    l10n,
    page::Page,
    session::Session,
    state::AppState,
};

/// `SubmissionsPreviewController::TTL`: how long after the last completion the preview is
/// open without a signed link.
const TTL: Duration = Duration::minutes(40);

/// `SubmissionsController::FIELD_ICONS`.
const FIELD_ICONS: &[(&str, &str)] = &[
    ("text", "text_size"),
    ("signature", "writing_sign"),
    ("date", "calendar_event"),
    ("number", "square_number_1"),
    ("image", "photo"),
    ("initials", "letter_case_upper"),
    ("file", "paperclip"),
    ("select", "select"),
    ("checkbox", "checkbox"),
    ("radio", "circle_dot"),
    ("stamp", "rubber_stamp"),
    ("cells", "columns_3"),
    ("multiple", "checks"),
    ("phone", "phone_check"),
    ("payment", "credit_card"),
    ("verification", "id"),
    ("kba", "user_scan"),
];

const BG_CLASSES: [&str; 10] = [
    "bg-red-100",
    "bg-sky-100",
    "bg-emerald-100",
    "bg-yellow-100",
    "bg-purple-100",
    "bg-pink-100",
    "bg-cyan-100",
    "bg-orange-100",
    "bg-lime-100",
    "bg-indigo-100",
];

const COLORS: [&str; 10] = [
    "bg-red-500",
    "bg-sky-500",
    "bg-emerald-500",
    "bg-yellow-300",
    "bg-purple-600",
    "bg-pink-500",
    "bg-cyan-500",
    "bg-orange-500",
    "bg-lime-500",
    "bg-indigo-500",
];

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Ruby truthiness of a JSON value (`nil` and `false` are falsy).
fn truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

/// `String#to_i`: the leading integer, 0 without one.
fn ruby_to_i(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<i64>().map_or(0, |n| sign * n)
}

/// `Integer#ordinalize`.
fn ordinalize(n: usize) -> String {
    let suffix = if (11..=13).contains(&(n % 100)) {
        "th"
    } else {
        match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{n}{suffix}")
}

/// `name.split(/(_)/)`: the parts and the underscores, trailing empty parts dropped.
fn title_parts(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for (i, part) in name.split('_').enumerate() {
        if i > 0 {
            parts.push("_".to_owned());
        }
        parts.push(part.to_owned());
    }
    while parts.last().is_some_and(String::is_empty) {
        parts.pop();
    }
    parts
}

fn title(form: &Form) -> String {
    form.submission
        .name
        .clone()
        .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default()
}

/// The "not supported yet" page (422) for a branch that is not ported.
fn unsupported(
    state: &AppState,
    session: &Session,
    locale: &str,
    name: &str,
    message: &str,
) -> Result<Response, AppError> {
    Page::new(state, session, locale)
        .status(StatusCode::UNPROCESSABLE_ENTITY)
        .with(json!({ "name": name, "message": message }))
        .render("submit_form/unsupported.html")
}

/// `Submission.find_by!(slug:)` with its submitters (the form of the first one).
async fn load_submission(state: &AppState, slug: &str) -> Result<Option<Form>, AppError> {
    let Some(id) = signing::submission_id_by_slug(&state.db, slug).await? else {
        return Ok(None);
    };
    let Some(first) = submissions::submitters_for(&state.db, &[id])
        .await?
        .into_iter()
        .next()
    else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "submission {id} has no submitters"
        )));
    };
    Form::for_submitter(state, first).await.map(Some)
}

/// The submission requires two-factor verification of a party.
fn requires_2fa(form: &Form) -> bool {
    let party = form.submitters.iter().any(|s| {
        truthy(s.preferences.get("require_phone_2fa"))
            || truthy(s.preferences.get("require_email_2fa"))
    });
    party
        || truthy(form.template_preference("require_phone_2fa"))
        || truthy(form.template_preference("require_email_2fa"))
}

/// The last completed party (`submitters.select(&:completed_at?).max_by(&:completed_at)`).
fn last_completed(form: &Form) -> Option<&Submitter> {
    let mut last: Option<&Submitter> = None;
    for s in &form.submitters {
        if let Some(at) = s.completed_at
            && last.is_none_or(|l| l.completed_at.is_some_and(|l| at > l))
        {
            last = Some(s);
        }
    }
    last
}

/// `SubmissionsPreviewController#use_signature?`.
fn use_signature(form: &Form, can_read: bool) -> bool {
    if can_read {
        return false;
    }
    if requires_2fa(form) {
        return true;
    }
    last_completed(form)
        .and_then(|s| s.completed_at)
        .is_none_or(|at| at <= (Utc::now() - TTL).naive_utc())
}

fn can_read(auth: Option<&Auth>, account_id: i64) -> bool {
    auth.is_some_and(|a| a.ability().can_account_record(account_id))
}

/// `GET /e/:slug`.
pub async fn show(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = pages::form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let sig = params.str("sig").filter(|s| !s.trim().is_empty());
    let mut signed = None;
    if let Some(sig) = sig.as_deref()
        && let Some(submitter) = downloads::signed_submitter(&state, sig).await?
    {
        let form = Form::for_submitter(&state, submitter).await?;
        if form.submission.slug == slug {
            signed = Some(form);
        }
    }
    let signature_valid = signed.is_some();
    let form = match signed {
        Some(form) => form,
        None => load_submission(&state, &slug)
            .await?
            .ok_or_else(not_found)?,
    };
    if form.account.archived_at.is_some() {
        return Err(not_found());
    }
    let can_read = can_read(auth.as_ref(), form.submission.account_id);
    if form.submission.completed_at.is_none() && !signature_valid && !can_read {
        return Err(not_found());
    }
    if !signature_valid && use_signature(&form, can_read) {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/e/{}/completed", form.submission.slug),
        ));
    }

    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if user_agent.contains("Hotwire Native") {
        return unsupported(
            &state,
            &session,
            &locale,
            &title(&form),
            "The submission page in the mobile app is not supported yet",
        );
    }
    let Some(documents) = pages::schema_documents(&state, &form).await? else {
        return unsupported(
            &state,
            &session,
            &locale,
            &title(&form),
            "Templates with dynamic documents are not supported yet",
        );
    };

    let origin = request_origin(&state, &headers);
    let sig_link = sig
        .filter(|_| signature_valid)
        .map(|sig| (form.submitter.slug.clone(), sig));
    let view = ShowView {
        state: &state,
        form: &form,
        auth: auth.as_ref(),
        can_read,
        origin: &origin,
        locale: &locale,
        sig: sig_link
            .as_ref()
            .map(|(slug, sig)| (slug.as_str(), sig.as_str())),
    };
    let mut vars = view.build(&documents).await?;
    vars["autosize_script"] = json!(headers.get("x-turbo-request-id").is_none());
    vars["nonce"] = json!(pages::csp_nonce());
    vars["robots_noindex"] = json!(true);
    Page::new(&state, &session, &locale)
        .auth(auth.as_ref())
        .with(vars)
        .render("submissions_preview/show.html")
}

/// What `submissions/show` reads besides the documents.
struct ShowView<'a> {
    state: &'a AppState,
    form: &'a Form,
    auth: Option<&'a Auth>,
    can_read: bool,
    origin: &'a str,
    locale: &'a str,
    /// The signed link's submitter slug and `sig`, when it is valid.
    sig: Option<(&'a str, &'a str)>,
}

/// `Rack::Utils.escape` of a query value (`to_query`).
fn query_escape(value: &str) -> String {
    const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'*')
        .remove(b'-')
        .remove(b'.')
        .remove(b'_');
    percent_encoding::utf8_percent_encode(value, COMPONENT)
        .to_string()
        .replace("%20", "+")
}

impl ShowView<'_> {
    fn signed_in(&self) -> bool {
        self.auth.is_some()
    }

    /// `l(time.in_time_zone(account.timezone), format:, locale: account.locale)`.
    fn account_time(&self, at: chrono::NaiveDateTime, format: &str) -> String {
        l10n::time(
            self.state,
            &self.form.account.locale,
            at,
            &self.form.account.timezone,
            format,
        )
    }

    fn t(&self, key: &str) -> String {
        self.state.views.t(self.locale, key, &[])
    }

    /// The `data-src` of a download button.
    fn download_src(&self, combined: bool, is_combined_enabled: bool) -> String {
        let slug = &self.form.submission.slug;
        if let Some((submitter_slug, sig)) = self.sig {
            // `{ sig:, combined: }` sorted by `to_query`; `compact_blank` drops `false`.
            let sig = query_escape(sig);
            return if combined || is_combined_enabled {
                format!("/s/{submitter_slug}/documents?combined=true&sig={sig}")
            } else {
                format!("/s/{submitter_slug}/documents?sig={sig}")
            };
        }
        if combined || is_combined_enabled {
            format!("/e/{slug}/download?combined=true")
        } else {
            format!("/e/{slug}/download")
        }
    }

    async fn build(&self, documents: &[Document]) -> Result<Value, AppError> {
        let state = self.state;
        let form = self.form;
        let submission = &form.submission;
        let configs = FormConfigs::load(state, form.account.id).await?;
        let combine = signing::account_configs(
            &state.db,
            form.account.id,
            &[super::audit::COMBINE_PDF_RESULT_KEY],
        )
        .await?
        .iter()
        .any(|(_, v)| *v == Value::Bool(true));
        let is_combined_enabled = combine
            && !form
                .fields()
                .iter()
                .any(|f| str_of(f, "type") == Some("verification"));
        let now = Utc::now().naive_utc();
        let expired = form.expired();
        let template_archived = form.template_archived();
        let signed_in = self.signed_in();
        // `can?(:read | :update | :destroy | :create, submission)`.
        let can_manage = self.can_read;
        let name = title(form);

        let mut header = json!({
            "title_parts": title_parts(&name),
            "home_href": match (&form.template, self.auth) {
                (Some(template), Some(auth)) if auth.account.id == submission.account_id => {
                    format!("/templates/{}", template.id)
                }
                _ => "/".to_owned(),
            },
            "unarchive_top": signed_in
                && can_manage
                && submission.archived_at.is_some()
                && submission.completed_at.is_none(),
            "submission_id": submission.id,
        });

        let audit_trail = Attachment::with_blobs_for_records(
            &state.db,
            "Submission",
            &[submission.id],
            "audit_trail",
        )
        .await?
        .into_iter()
        .next();
        if let Some((_, mut blob)) = audit_trail {
            let uuid = blob.ensure_uuid(&state.db).await?;
            header["audit_log_url"] = json!(crate::files::path_for(
                state,
                &uuid,
                &blob.filename,
                Some(Utc::now() + Duration::hours(4)),
            ));
        } else if signed_in {
            header["event_log"] = json!(true);
        }

        if form.submitters.iter().any(|s| s.completed_at.is_some()) {
            let show_combined = submission.completed_at.is_some() && !is_combined_enabled;
            let show_unarchive = signed_in
                && can_manage
                && submission.archived_at.is_some()
                && submission.completed_at.is_some();
            if submission.completed_at.is_some() || !is_combined_enabled {
                header["download"] = json!({
                    "src": self.download_src(false, is_combined_enabled),
                    "dropdown": show_combined || show_unarchive,
                    "combined_src": show_combined.then(|| self.download_src(true, is_combined_enabled)),
                    "unarchive": show_unarchive,
                });
            }
        } else if let [only] = form.submitters.as_slice()
            && !expired
            && only.declined_at.is_none()
            && submission.archived_at.is_none()
            && !template_archived
        {
            header["share_link"] = json!(format!("{}/s/{}", state.default_url(), only.slug));
        }

        let values = form.all_values();
        let option_word = self.t("option");
        let schema = rules::filtered_conditions_schema(
            form.schema(),
            form.fields(),
            &values,
            None,
            &option_word,
        );
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

        let (thumbnails, rendered) = self.documents(
            documents,
            &schema,
            &values,
            &attachments,
            &configs,
            can_manage,
        );
        let parties = self
            .parties(&values, &attachments, &configs, can_manage, now)
            .await?;

        Ok(json!({
            "header": header,
            "thumbnails": thumbnails,
            "documents": rendered,
            "parties": parties,
        }))
    }

    /// The value shown for a field: the parties' value, or a read-only default.
    fn field_value(
        &self,
        values: &serde_json::Map<String, Value>,
        field: &Value,
        check_conditions: bool,
    ) -> Option<Value> {
        let uuid = str_of(field, "uuid").unwrap_or_default();
        if let Some(value) = values.get(uuid).filter(|v| present(Some(v))) {
            return Some(value.clone());
        }
        let default = field.get("default_value")?;
        if default.as_str() == Some("{{date}}")
            || field.get("readonly") != Some(&Value::Bool(true))
            || (check_conditions && present(field.get("conditions")))
            || blank(Some(default))
        {
            return None;
        }
        let owner = self
            .form
            .submitters
            .iter()
            .find(|s| Some(s.uuid.as_str()) == str_of(field, "submitter_uuid"))?;
        default_value_for(self.state, self.form, owner, default, field, false)
    }

    fn masked(value: &Value, mask: &Value) -> String {
        wrap(Some(value))
            .iter()
            .map(|v| mask_value(&ruby_to_s(v), mask))
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[allow(
        clippy::too_many_lines,
        reason = "mirrors the document loop of the view"
    )]
    fn documents(
        &self,
        documents: &[Document],
        schema: &[Value],
        values: &serde_json::Map<String, Value>,
        attachments: &HashMap<String, (Attachment, Blob)>,
        configs: &FormConfigs,
        can_manage: bool,
    ) -> (Vec<Value>, Vec<Value>) {
        let state = self.state;
        let form = self.form;
        let font_scale = 1040.0 / US_LETTER_W;
        let fields_index = rules::field_areas_index(form.fields());
        let order_index: HashMap<&str, usize> = form
            .template_submitters()
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((str_of(s, "uuid")?, i)))
            .collect();
        let mut thumbnails = Vec::new();
        let mut rendered = Vec::new();
        for item in schema {
            let Some(document) = documents
                .iter()
                .find(|d| Some(d.attachment.uuid.as_str()) == str_of(item, "attachment_uuid"))
            else {
                continue;
            };
            let item_name = str_of(item, "name").unwrap_or_default();
            let display_name = if item_name.trim().is_empty() {
                crate::api::serialize::filename_base(&document.blob.filename)
            } else {
                item_name.to_owned()
            };
            if let Some((_, first)) = document.previews.first() {
                let (_, image) = document
                    .previews
                    .iter()
                    .find(|(_, b)| {
                        ruby_to_i(&crate::api::serialize::filename_base(&b.filename)) == 0
                    })
                    .unwrap_or(&document.previews[0]);
                let m = first.metadata_json();
                thumbnails.push(json!({
                    "uuid": document.attachment.uuid,
                    "src": crate::disk::blob_url(state, self.origin, image),
                    "width": m.get("width"),
                    "height": m.get("height"),
                    "name": display_name,
                }));
            }

            let metadata = document.blob.metadata_json();
            let annots = metadata
                .pointer("/pdf/annotations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let preview_index: HashMap<i64, &Blob> = document
                .previews
                .iter()
                .map(|(_, b)| {
                    (
                        ruby_to_i(&crate::api::serialize::filename_base(&b.filename)),
                        b,
                    )
                })
                .collect();
            let lazy = document.previews.first().map_or_else(
                || json!({ "width": US_LETTER_SIZE.0, "height": US_LETTER_SIZE.1 }),
                |(_, b)| b.metadata_json(),
            );
            let pages = metadata
                .pointer("/pdf/number_of_pages")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| i64::try_from(preview_index.len()).unwrap_or_default());
            let signed_key = state.secrets.signed_id_verifier.generate(
                &json!([document.attachment.id, document.attachment.uuid]),
                Some("attachment"),
                Some(Utc::now() + Duration::hours(6)),
            );
            let mut page_views = Vec::new();
            for index in 0..pages {
                let (url, width, height) = match preview_index.get(&index) {
                    Some(blob) => {
                        let m = blob.metadata_json();
                        (
                            crate::disk::blob_url(state, self.origin, blob),
                            m.get("width").cloned().unwrap_or(Value::Null),
                            m.get("height").cloned().unwrap_or(Value::Null),
                        )
                    }
                    None => (
                        format!("/preview/{signed_key}/{index}.jpg"),
                        lazy.get("width").cloned().unwrap_or(Value::Null),
                        lazy.get("height").cloned().unwrap_or(Value::Null),
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
                let mut fields = Vec::new();
                for (area, field) in fields_index
                    .get(&document.attachment.uuid)
                    .and_then(|pages| pages.get(&index))
                    .into_iter()
                    .flatten()
                {
                    let field_type = str_of(field, "type").unwrap_or_default();
                    let mut value = self.field_value(values, field, true);
                    if value.is_none() && field_type == "heading" {
                        value = field.get("default_value").cloned();
                    }
                    let submitter = form
                        .submitters
                        .iter()
                        .find(|s| Some(s.uuid.as_str()) == str_of(field, "submitter_uuid"));
                    if field_type == "signature"
                        && submitter.is_some_and(|s| s.completed_at.is_none())
                    {
                        value = None;
                    }
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
                    let partial = |value: &Value, locals: &ValueLocals<'_>| {
                        pages::value_partial(
                            state,
                            attachments,
                            self.origin,
                            locals,
                            area,
                            field,
                            value,
                            submitter,
                        )
                    };
                    if let Some(value) = value.filter(|v| present(Some(v))) {
                        let mask = field
                            .pointer("/preferences/mask")
                            .filter(|m| present(Some(m)));
                        match mask {
                                Some(mask) if self.signed_in() && can_manage => {
                                    // The hover pair renders without the page size and
                                    // the time options.
                                    let hover = ValueLocals {
                                        with_submitter_timezone: false,
                                        with_timestamp_seconds: false,
                                        with_signature_id_reason: true,
                                        with_signature_id_completed_at: false,
                                        ..locals
                                    };
                                    fields.push(json!({
                                        "kind": "masked",
                                        "value": partial(&value, &hover),
                                        "masked": partial(&Value::String(Self::masked(&value, mask)), &hover),
                                    }));
                                }
                                Some(mask) => fields.push(json!({
                                    "kind": "value",
                                    "value": partial(&Value::String(Self::masked(&value, mask)), &locals),
                                })),
                                None => fields.push(json!({
                                    "kind": "value",
                                    "value": partial(&value, &locals),
                                })),
                            }
                    } else {
                        if field.get("readonly") == Some(&Value::Bool(true)) {
                            continue;
                        }
                        let Some(submitter) = submitter.filter(|s| s.completed_at.is_none()) else {
                            continue;
                        };
                        let index = order_index
                            .get(submitter.uuid.as_str())
                            .copied()
                            .unwrap_or(0);
                        let icon = FIELD_ICONS
                            .iter()
                            .find(|(t, _)| *t == field_type)
                            .map_or("text_size", |(_, icon)| icon);
                        fields.push(json!({
                            "kind": "placeholder",
                            "style": format!(
                                "width: {}%; height: {}%; left: {}%; top: {}%;",
                                ruby_float(float(area.get("w")) * 100.0),
                                ruby_float(float(area.get("h")) * 100.0),
                                ruby_float(float(area.get("x")) * 100.0),
                                ruby_float(float(area.get("y")) * 100.0),
                            ),
                            "bg_class": BG_CLASSES[index % BG_CLASSES.len()],
                            "icon": icon,
                        }));
                    }
                }
                page_views.push(json!({
                    "index": index,
                    "width": width,
                    "height": height,
                    "url": url,
                    "alt": format!("{item_name} - {} {}", self.t("page"), index + 1),
                    "annots": page_annots,
                    "fields": fields,
                }));
            }
            rendered.push(json!({ "uuid": document.attachment.uuid, "pages": page_views }));
        }
        (thumbnails, rendered)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "mirrors the parties loop of the view"
    )]
    async fn parties(
        &self,
        values: &serde_json::Map<String, Value>,
        attachments: &HashMap<String, (Attachment, Blob)>,
        configs: &FormConfigs,
        can_manage: bool,
        now: chrono::NaiveDateTime,
    ) -> Result<Vec<Value>, AppError> {
        let state = self.state;
        let form = self.form;
        let submission = &form.submission;
        let expired = form.expired();
        let template_archived = form.template_archived();
        let signed_in = self.signed_in();
        let can_send_emails = signed_in && crate::mail::can_send_emails(state).await?;
        let mut fields_by_party: HashMap<&str, Vec<&Value>> = HashMap::new();
        for field in form.fields() {
            fields_by_party
                .entry(str_of(field, "submitter_uuid").unwrap_or_default())
                .or_default()
                .push(field);
        }
        let mut counters: HashMap<String, usize> = HashMap::new();
        let mut parties = Vec::new();
        for (index, item) in form.template_submitters().iter().enumerate() {
            let item_uuid = str_of(item, "uuid");
            let submitter = form
                .submitters
                .iter()
                .find(|s| Some(s.uuid.as_str()) == item_uuid);
            let role = submitter.and_then(|s| form.role(&s.uuid)).map_or_else(
                || format!("{} Submitter", ordinalize(index + 1)),
                str::to_owned,
            );

            let mut party = json!({
                "color": COLORS[index % COLORS.len()],
                "role": role,
            });
            if let Some(s) = submitter {
                let open = s.completed_at.is_none()
                    && submission.completed_at.is_none()
                    && submission.archived_at.is_none()
                    && !template_archived
                    && !expired
                    && s.declined_at.is_none();
                let viewer = form.is_viewer(s);
                party["name"] = json!(s.name.as_deref().filter(|n| !n.trim().is_empty()));
                party["email"] = json!(s.email.as_deref().filter(|e| !e.trim().is_empty()));
                party["phone"] = json!(s.phone.as_deref().filter(|p| !p.trim().is_empty()));
                party["edit"] = json!(
                    signed_in
                        && can_manage
                        && open
                        && !signing::has_event(&state.db, s.id, &["start_form"]).await?
                );
                party["id"] = json!(s.id);
                party["slug"] = json!(s.slug);
                let (icon, status) = if viewer {
                    (
                        if s.opened_at.is_some() {
                            "file_check"
                        } else {
                            "file_unknown"
                        },
                        match s.opened_at {
                            Some(at) => self.state.views.t(
                                self.locale,
                                "viewed_on_time",
                                &[("time", self.account_time(at, "short"))],
                            ),
                            None if submission.completed_at.is_some() => self.t("not_viewed"),
                            None => self.t("not_viewed_yet"),
                        },
                    )
                } else {
                    let icon = if s.completed_at.is_none() && submission.completed_at.is_some() {
                        "writing_off"
                    } else if submission.expire_at.is_some() && s.completed_at.is_none() {
                        "clock_exclamation"
                    } else {
                        "writing"
                    };
                    let status = if let Some(at) = s.completed_at {
                        self.account_time(at, "long")
                    } else if submission.completed_at.is_some() {
                        self.t("not_completed")
                    } else if let Some(expire_at) = submission.expire_at {
                        if expired {
                            self.t("expired")
                        } else {
                            self.state.views.t(
                                self.locale,
                                "expire_on_time",
                                &[("time", self.account_time(expire_at, "short"))],
                            )
                        }
                    } else {
                        self.t("not_completed_yet")
                    };
                    (icon, status)
                };
                let status = match s.declined_at {
                    Some(at) => self.state.views.t(
                        self.locale,
                        "declined_on_time",
                        &[("time", self.account_time(at, "short"))],
                    ),
                    None => status,
                };
                party["icon"] = json!(icon);
                party["status"] = json!(status);
                if s.declined_at.is_some() {
                    let reason = signing::events_of_types(&state.db, s.id, &["decline_form"])
                        .await?
                        .into_iter()
                        .next()
                        .and_then(|(_, data)| data.get("reason").map(ruby_to_s))
                        .unwrap_or_default();
                    party["decline_reason_html"] = json!(crate::mail::simple_format(
                        &loco_views::escape_html(&reason)
                    ));
                }
                party["send_email"] = json!(
                    (signed_in && s.email.is_some() && open && can_manage && can_send_emails).then(
                        || self.t(if s.sent_at.is_some() {
                            "re_send_email"
                        } else {
                            "send_email"
                        })
                    )
                );
                party["send_sms"] = json!(
                    (signed_in && s.phone.is_some() && open && can_manage).then(|| self.t(
                        if s.sent_at.is_some() {
                            "re_send_sms"
                        } else {
                            "send_sms"
                        }
                    ))
                );
                party["sign_in_person"] = json!(signed_in && !viewer && open && can_manage);
                party["resubmit"] = json!(
                    self.auth
                        .is_some_and(|a| s.email.as_deref() == Some(a.user.email.as_str()))
                        && s.completed_at
                            .is_some_and(|at| Some(at) > now.checked_sub_months(Months::new(1)))
                        && can_manage
                );
            } else {
                party["icon"] = json!("writing");
                party["status"] = json!(self.t("not_invited_yet"));
            }

            let mut items = Vec::new();
            for field in fields_by_party
                .get(item_uuid.unwrap_or_default())
                .into_iter()
                .flatten()
            {
                let field_type = str_of(field, "type").unwrap_or_default();
                let counter = {
                    let c = counters.entry(field_type.to_owned()).or_default();
                    *c += 1;
                    *c
                };
                let Some(value) = self
                    .field_value(values, field, false)
                    .filter(|v| present(Some(v)))
                else {
                    continue;
                };
                if matches!(field_type, "heading" | "strikethrough")
                    || (field_type == "signature"
                        && submitter.is_some_and(|s| s.completed_at.is_none()))
                {
                    continue;
                }
                let label = str_of(field, "name")
                    .filter(|n| !n.trim().is_empty())
                    .map_or_else(
                        || format!("{} {counter}", self.t(&format!("{field_type}_field"))),
                        str::to_owned,
                    );
                let alt = field
                    .get("name")
                    .filter(|v| !v.is_null())
                    .or_else(|| field.get("title").filter(|v| !v.is_null()))
                    .map_or_else(|| field_type.to_owned(), ruby_to_s);
                let value_str = value.as_str().unwrap_or_default();
                let attachment = attachments.get(value_str);
                let image = |max_height: i64| -> Option<Value> {
                    let (_, blob) = attachment?;
                    let m = blob.metadata_json();
                    let height = m.get("height").filter(|v| !v.is_null());
                    Some(json!({
                        "style": format!(
                            "{}height: {max_height}px",
                            if height.is_some() { "max-" } else { "" }
                        ),
                        "height": height,
                        "width": m.get("width"),
                        "src": crate::disk::blob_url(state, self.origin, blob),
                    }))
                };
                let is_image = attachment.is_some_and(|(_, b)| {
                    b.content_type
                        .as_deref()
                        .is_some_and(|c| c.starts_with("image/"))
                });
                let mut entry = json!({ "label": label, "alt": alt });
                match field_type {
                    "signature" | "initials" => {
                        entry["kind"] = json!("image");
                        entry["image"] =
                            json!(image(if field_type == "signature" { 100 } else { 50 }));
                    }
                    "image" | "stamp" | "kba" if is_image => {
                        entry["kind"] = json!("image");
                        entry["bare"] = json!(true);
                        entry["image"] = json!(image(200));
                    }
                    "file" | "payment" | "image" => {
                        let files: Vec<Value> = wrap(Some(&value))
                            .iter()
                            .filter_map(|v| {
                                let (_, blob) = attachments.get(v.as_str()?)?;
                                Some(json!({
                                    "url": crate::disk::blob_url(state, self.origin, blob),
                                    "filename": blob.filename,
                                }))
                            })
                            .collect();
                        entry["kind"] = json!("files");
                        entry["files"] = json!(files);
                    }
                    "checkbox" => entry["kind"] = json!("check"),
                    _ => {
                        let value = match field_type {
                            "number" => Value::String(l10n::format_number(
                                state,
                                &value,
                                field.pointer("/preferences/format").and_then(Value::as_str),
                            )),
                            "date" => {
                                let timezone = submitter
                                    .and_then(|s| s.timezone.clone())
                                    .filter(|t| {
                                        configs.with_submitter_timezone && !t.trim().is_empty()
                                    })
                                    .unwrap_or_else(|| form.account.timezone.clone());
                                Value::String(l10n::format_date_string(
                                    state,
                                    &ruby_to_s(&value),
                                    field.pointer("/preferences/format").and_then(Value::as_str),
                                    &form.account.locale,
                                    &timezone,
                                ))
                            }
                            _ => value,
                        };
                        let joined = wrap(Some(&value))
                            .iter()
                            .map(ruby_to_s)
                            .collect::<Vec<_>>()
                            .join(", ");
                        entry["kind"] = json!("text");
                        match field
                            .pointer("/preferences/mask")
                            .filter(|m| present(Some(m)))
                        {
                            Some(mask) if signed_in && can_manage => {
                                entry["kind"] = json!("masked");
                                entry["masked"] = json!(Self::masked(&value, mask));
                                entry["text"] = json!(joined);
                            }
                            Some(mask) => entry["text"] = json!(Self::masked(&value, mask)),
                            None => entry["text"] = json!(joined),
                        }
                    }
                }
                items.push(entry);
            }
            party["fields"] = json!(items);
            parties.push(party);
        }
        Ok(parties)
    }
}

/// `GET /e/:slug/completed`.
pub async fn completed(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = pages::form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let form = load_submission(&state, &slug)
        .await?
        .ok_or_else(not_found)?;
    if form.account.archived_at.is_some() {
        return Err(not_found());
    }
    // `submitters.completed.order(:completed_at).last`
    let signed_text = form
        .submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at)
        .map(|last| {
            let with_signature_fields = form.fields().iter().any(|f| {
                str_of(f, "submitter_uuid") == Some(last.uuid.as_str())
                    && matches!(str_of(f, "type"), Some("signature" | "initials"))
            });
            let time = l10n::local_date(
                &state,
                &locale,
                last.completed_at.unwrap_or_default(),
                &form.account.timezone,
                "long",
            );
            state.views.t(
                &locale,
                if with_signature_fields {
                    "signed_on_time"
                } else {
                    "completed_on_time"
                },
                &[("time", time)],
            )
        });
    let email = auth
        .as_ref()
        .map(|a| a.user.email.clone())
        .or_else(|| params.get("email").map(ruby_to_s));
    Page::new(&state, &session, &locale)
        .auth(auth.as_ref())
        .with(json!({
            "name": title(&form),
            "signed_text": signed_text,
            "can_email": crate::mail::can_send_emails(&state).await?,
            "submission_slug": form.submission.slug,
            "email": email,
            "status_error": params.str("status").as_deref() == Some("error"),
            "robots_noindex": true,
        }))
        .render("submissions_preview/completed.html")
}

/// `GET /e/:slug/download`: links to the result documents of the last completed party.
pub async fn download(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    params: Params,
) -> Result<Response, AppError> {
    let form = load_submission(&state, &slug)
        .await?
        .ok_or_else(not_found)?;
    // `submitters.where.not(completed_at: nil).order(:completed_at).last`
    let Some(last) = form
        .submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at)
        .cloned()
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    super::results::ensure_result_generated(&state, &last).await?;
    if !can_read(auth.as_ref(), form.submission.account_id) {
        if requires_2fa(&form) {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        if last
            .completed_at
            .is_some_and(|at| at < (Utc::now() - TTL).naive_utc())
        {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
    }
    let last_form = Form::for_submitter(&state, last.clone()).await?;
    if params.str("combined").as_deref() == Some("true") {
        // `build_combined_url` is nil until the submission completes, and for any party
        // but the last one.
        if last_form.submission.completed_at.is_none() {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        return Ok(downloads::combined_unsupported());
    }
    if downloads::uses_combined(&state, &last_form).await? {
        return Ok(downloads::combined_unsupported());
    }
    let urls = downloads::build_document_urls(&state, last).await?;
    Ok(crate::api::ok(&urls))
}

/// `GET /p/:slug`: the page a phone opens from the signature QR code.
pub async fn draw_signature(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = pages::form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let form = Form::load(&state, &slug).await?.ok_or_else(not_found)?;
    if form.submitter.completed_at.is_some() {
        return Ok(redirect_to(
            &state,
            &headers,
            &format!("/s/{}/completed", form.submitter.slug),
        ));
    }
    let form_path = format!("/s/{}", form.submitter.slug);
    if form.viewer() {
        return Ok(redirect_to(&state, &headers, &form_path));
    }
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if form.template_archived()
        || form.submission.archived_at.is_some()
        || !access.authorized(&form)
        || !access.pass_submitters_order(&form).await?
    {
        return Ok(redirect_to(&state, &headers, &form_path));
    }
    // `f['uuid'].starts_with?(params[:f])`: Rails fails without `f` or a matching field.
    let Some(prefix) = params.get("f").map(ruby_to_s) else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "no signature field given"
        )));
    };
    let Some(field) = form.fields().iter().find(|f| {
        str_of(f, "type") == Some("signature")
            && str_of(f, "uuid").is_some_and(|u| u.starts_with(prefix.as_str()))
    }) else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "no signature field starts with {prefix:?}"
        )));
    };
    Page::new(&state, &session, &locale)
        .with(json!({
            "slug": slug,
            "color": field.pointer("/preferences/color").map(ruby_to_s),
            "field_uuid": str_of(field, "uuid"),
            "form_path": form_path,
            "robots_noindex": true,
        }))
        .render("submit_form_draw_signature/show.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_titles_like_ruby() {
        assert_eq!(title_parts("Parity NDA"), ["Parity NDA"]);
        assert_eq!(title_parts("a_b"), ["a", "_", "b"]);
        assert_eq!(title_parts("_a"), ["", "_", "a"]);
        assert_eq!(title_parts("a_"), ["a", "_"]);
        assert_eq!(title_parts(""), Vec::<String>::new());
    }

    #[test]
    fn ordinalizes_like_active_support() {
        let words: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101, 111]
            .into_iter()
            .map(ordinalize)
            .collect();
        assert_eq!(
            words,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st",
                "111th"
            ]
        );
    }

    #[test]
    fn parses_leading_integers_like_ruby() {
        assert_eq!(ruby_to_i("0"), 0);
        assert_eq!(ruby_to_i("12"), 12);
        assert_eq!(ruby_to_i("3abc"), 3);
        assert_eq!(ruby_to_i("-2"), -2);
        assert_eq!(ruby_to_i("abc"), 0);
    }

    #[test]
    fn escapes_query_values_like_cgi() {
        assert_eq!(query_escape("ab--12_x.y-z*"), "ab--12_x.y-z*");
        assert_eq!(query_escape("a=b/c+d e"), "a%3Db%2Fc%2Bd+e");
    }

    #[test]
    fn ruby_truthiness() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(false))));
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!(""))));
        assert!(truthy(Some(&json!(0))));
    }
}
