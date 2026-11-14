//! `SubmitFormController#update` and `Submitters::SubmitValues.call`.

use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use loco_db::{
    JsonText,
    signing::{self, SubmitterProgress},
    submissions,
};
use loco_domain::submit::{self as rules, Casts, blank, present};
use serde_json::{Map, Value, json};

use super::{Access, Form, completion};
use crate::{
    api::Params,
    auth::MaybeAuth,
    errors::AppError,
    locale,
    request::ClientIp,
    state::AppState,
    webhooks::{self, EventRecord},
};

/// Why a submission was refused: a missing required field, or a message.
#[derive(Debug)]
pub enum SubmitError {
    Required(String),
    Invalid(String),
}

impl From<rules::ValidationError> for SubmitError {
    fn from(err: rules::ValidationError) -> Self {
        Self::Invalid(err.0)
    }
}

fn error(message: &str) -> Response {
    crate::api::render(
        StatusCode::UNPROCESSABLE_ENTITY,
        &json!({ "error": message }),
    )
}

/// `PUT /s/:slug`.
pub async fn update(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: crate::session::Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = locale::app_locale(&state, auth.as_ref(), None);
    let t = |key: &str| state.views.t(&locale, key, &[]);
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if !access.authorized(&form) {
        return Ok(error(&t(
            "verification_required_refresh_the_page_and_pass_2fa",
        )));
    }
    if form.submitter.completed_at.is_some() {
        return Ok(error(&t("form_has_been_completed_already")));
    }
    if form.template_archived() || form.submission.archived_at.is_some() {
        return Ok(error(&t("form_has_been_archived")));
    }
    if form.expired() {
        return Ok(error(&t("form_has_been_expired")));
    }
    if form.submitter.declined_at.is_some() {
        return Ok(error(&t("form_has_been_declined")));
    }
    if form.viewer() {
        return Ok(error(&t("form_is_view_only")));
    }

    let request = RequestInfo {
        ip,
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        headers: headers.clone(),
        sid: Some(session.public_id()),
        user_id: auth.as_ref().map(|a| a.true_user.id),
        locale: locale.clone(),
    };
    match submit_values(&state, form, &params.0, &request, true).await? {
        Ok(()) => Ok(StatusCode::OK.into_response()),
        Err(SubmitError::Required(uuid)) => Ok(crate::api::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({ "field_uuid": uuid }),
        )),
        Err(SubmitError::Invalid(message)) => Ok(error(&message)),
    }
}

/// What `create_with_tracking_data` and `assign_completed_attributes` read from the request.
pub struct RequestInfo {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub headers: HeaderMap,
    /// `request.session.id`.
    pub sid: Option<String>,
    pub user_id: Option<i64>,
    /// `I18n.locale` of the request.
    pub locale: String,
}

impl RequestInfo {
    pub async fn track(
        &self,
        state: &AppState,
        submitter: &submissions::Submitter,
        event_type: &str,
        data: Option<Map<String, Value>>,
    ) -> Result<(), AppError> {
        let data = crate::api::submitters::tracking_data(
            self.ip.as_deref(),
            self.user_agent.as_deref(),
            self.sid.as_deref(),
            self.user_id,
            data.unwrap_or_default(),
        );
        submissions::insert_event(
            &state.db,
            submitter,
            event_type,
            &JsonText(Value::Object(data)),
        )
        .await?;
        Ok(())
    }
}

fn param_true(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_str) == Some("true")
}

/// `Submitters::SubmitValues.call`.
pub async fn submit_values(
    state: &AppState,
    mut form: Form,
    params: &Value,
    request: &RequestInfo,
    validate_required: bool,
) -> Result<Result<(), SubmitError>, AppError> {
    if form
        .submission
        .template_fields
        .as_ref()
        .is_none_or(|f| blank(Some(f)))
        && let Some(template) = &form.template
    {
        // `Submissions.update_template_fields!`.
        submissions::update_snapshots(
            &state.db,
            form.submission.id,
            Some(&template.fields),
            Some(&template.schema),
            template.variables_schema.as_ref(),
        )
        .await?;
        if form
            .submission
            .template_submitters
            .as_ref()
            .is_none_or(|s| blank(Some(s)))
        {
            signing::set_template_submitters(&state.db, form.submission.id, &template.submitters)
                .await?;
        }
        form = form.reload(state).await?;
    }

    if !signing::has_event(&state.db, form.submitter.id, &["start_form"]).await? {
        request
            .track(state, &form.submitter, "start_form", None)
            .await?;
        webhooks::enqueue(
            state,
            "form.started",
            &[EventRecord {
                id: form.submitter.id,
                account_id: form.submitter.account_id,
            }],
        )
        .await;
    }

    let completed =
        match update_submitter(state, &mut form, params, request, validate_required).await? {
            Ok(completed) => completed,
            Err(err) => return Ok(Err(err)),
        };

    if completed {
        let viewer_uuids: Vec<String> = form
            .template_submitters()
            .iter()
            .filter(|s| present(s.get("is_viewer")))
            .filter_map(|s| s.get("uuid").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let is_last =
            signing::maybe_update_completed_at(&state.db, form.submission.id, &viewer_uuids)
                .await?;
        completion::enqueue(state, form.submitter.id, Some(is_last)).await?;
    }
    Ok(Ok(()))
}

/// `SubmitValues.update_submitter!`. Returns whether the submitter completed the form.
async fn update_submitter(
    state: &AppState,
    form: &mut Form,
    params: &Value,
    request: &RequestInfo,
    validate_required: bool,
) -> Result<Result<bool, SubmitError>, AppError> {
    let submitted = params
        .get("values")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let values = rules::normalized_values(
        &submitted,
        Casts {
            boolean: param_true(params, "cast_boolean"),
            number: param_true(params, "cast_number"),
            phone: param_true(params, "normalize_phone"),
        },
    );

    let mut merged = form
        .submitter
        .values
        .as_object()
        .cloned()
        .unwrap_or_default();
    merged.extend(values.clone());
    let now = Utc::now().naive_utc();
    let completed = param_true(params, "completed");
    let mut progress = SubmitterProgress {
        values: JsonText(Value::Object(merged)),
        opened_at: form.submitter.opened_at.or(Some(now)),
        completed_at: form.submitter.completed_at,
        ip: form.submitter.ip.clone(),
        ua: form.submitter.ua.clone(),
        timezone: form.submitter.timezone.clone(),
    };
    if completed {
        progress.completed_at = Some(now);
        progress.ip.clone_from(&request.ip);
        progress.ua.clone_from(&request.user_agent);
        progress.timezone = params
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::to_owned);
        form.submitter.values = progress.values.clone();
        match assign_completed_values(state, form, Some(validate_required)).await? {
            Ok(values) => progress.values = JsonText(Value::Object(values)),
            Err(err) => return Ok(Err(err)),
        }
    }

    let mut tx = state.db.begin().await?;
    let reason_label = state.views.t(&request.locale, "reason", &[]);
    let reason_uuid =
        match maybe_set_signature_reason(&mut tx, form, &values, params, &reason_label).await? {
            Ok(uuid) => uuid,
            Err(err) => return Ok(Err(err)),
        };
    let option_word = form.option_word(state);
    for (key, value) in &values {
        if reason_uuid.as_deref() == Some(key.as_str()) {
            continue;
        }
        let field = form
            .fields()
            .iter()
            .find(|f| f.get("uuid").and_then(Value::as_str) == Some(key));
        if let Err(err) = rules::validate_value(value, field, &form.submitter.uuid, &option_word) {
            return Ok(Err(err.into()));
        }
    }
    if completed {
        if pending_invite(form) {
            return Ok(Err(SubmitError::Invalid("Invite required".into())));
        }
        progress.values = JsonText(Value::Object(
            invite_via_field(
                &mut tx,
                state,
                form,
                progress.values.as_object().cloned().unwrap_or_default(),
                request,
            )
            .await?,
        ));
    }
    let saved = signing::save_progress(&mut *tx, form.submitter.id, &progress).await?;
    if completed {
        // The event is created inside the transaction in Rails; it only needs the ids.
        let data = event_data(request);
        submissions::insert_event(
            &mut *tx,
            &saved,
            "complete_form",
            &JsonText(Value::Object(data)),
        )
        .await?;
    }
    tx.commit().await?;
    form.submitter = saved;

    if completed {
        crate::search::reindex_later(state, "Submitter", &[form.submitter.id]).await;
    }
    Ok(Ok(completed))
}

fn event_data(request: &RequestInfo) -> Map<String, Value> {
    crate::api::submitters::tracking_data(
        request.ip.as_deref(),
        request.user_agent.as_deref(),
        request.sid.as_deref(),
        request.user_id,
        Map::new(),
    )
}

/// `assign_completed_attributes`: default, formula and date values, condition filtering,
/// and the required-field check (none for `None`, as the API completes submitters).
/// Returns the final values.
pub(crate) async fn assign_completed_values(
    state: &AppState,
    form: &Form,
    validate_required: Option<bool>,
) -> Result<Result<Map<String, Value>, SubmitError>, AppError> {
    let values = match merge_default_values(state, form).await? {
        Ok(values) => values,
        Err(err) => return Ok(Err(err)),
    };
    let option_word = form.option_word(state);
    let fields = form.fields();
    let has_document_conditions = form
        .schema()
        .iter()
        .any(|item| present(item.get("conditions")));
    let other_values = form.other_values();
    let visible: Option<HashSet<String>> = has_document_conditions.then(|| {
        let mut all = other_values.clone();
        all.extend(values.clone());
        rules::filtered_conditions_schema(form.schema(), fields, &all, None, &option_word)
            .iter()
            .filter_map(|i| {
                i.get("attachment_uuid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    });
    let has_other = form.template_submitters().len() > 1;
    let (mut values, mut required) = rules::remove_condition_values(
        fields,
        &form.submitter.uuid,
        &values,
        &other_values,
        visible.as_ref(),
        has_other,
        &option_word,
    );

    // `build_formula_values`: the open-source build evaluates formulas to 0, nil or ''.
    let formula_values: Map<String, Value> = fields
        .iter()
        .filter(|f| f.get("submitter_uuid").and_then(Value::as_str) == Some(&form.submitter.uuid))
        .filter(|f| f.get("type").and_then(Value::as_str) != Some("payment"))
        .filter(|f| present(f.pointer("/preferences/formula")))
        .filter_map(|f| {
            let uuid = f.get("uuid").and_then(Value::as_str)?.to_owned();
            match f.get("type").and_then(Value::as_str) {
                Some("text" | "date") => None,
                _ => Some((uuid, json!(0))),
            }
        })
        .collect();
    if !formula_values.is_empty() {
        values.extend(formula_values);
        let (filtered, again) = rules::remove_condition_values(
            fields,
            &form.submitter.uuid,
            &values,
            &other_values,
            visible.as_ref(),
            has_other,
            &option_word,
        );
        values = filtered;
        required.retain(|uuid| again.contains(uuid));
    }

    // `replace_current_date_placeholders`.
    let tz = crate::api::time::account_zone(&form.account.timezone);
    let now = Utc::now();
    for (uuid, value) in &mut values {
        if value.as_str() == Some("{{date}}") {
            let format = fields
                .iter()
                .find(|f| f.get("uuid").and_then(Value::as_str) == Some(uuid))
                .and_then(|f| f.pointer("/preferences/format"))
                .and_then(Value::as_str);
            *value = Value::String(rules::current_date_value(format, now, tz));
        }
    }

    let Some(validate_required) = validate_required else {
        return Ok(Ok(values));
    };
    let index = rules::fields_uuid_index(fields);
    for uuid in &required {
        if present(values.get(uuid)) {
            continue;
        }
        let field_type = index
            .get(uuid)
            .and_then(|f| f.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if validate_required || rules::REQUIRED_FIELD_TYPES.contains(&field_type) {
            return Ok(Err(SubmitError::Required(uuid.clone())));
        }
    }
    Ok(Ok(values))
}

/// `merge_default_values`: stamps, verification results and template defaults, under the
/// submitted values.
async fn merge_default_values(
    state: &AppState,
    form: &Form,
) -> Result<Result<Map<String, Value>, SubmitError>, AppError> {
    let mut defaults = Map::new();
    for field in form.fields() {
        if field.get("submitter_uuid").and_then(Value::as_str) != Some(&form.submitter.uuid) {
            continue;
        }
        let uuid = field
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match field.get("type").and_then(Value::as_str) {
            Some("stamp") => {
                return Ok(Err(SubmitError::Invalid(
                    "Stamp fields are not supported yet".into(),
                )));
            }
            Some("verification") => {
                if signing::has_event(&state.db, form.submitter.id, &["complete_verification"])
                    .await?
                {
                    defaults.insert(uuid.to_owned(), json!(state.views.t("en", "verified", &[])));
                } else if present(field.get("required")) {
                    return Ok(Err(SubmitError::Invalid("ID Not Verified".into())));
                }
                continue;
            }
            _ => {}
        }
        let Some(value) = field.get("default_value").filter(|v| present(Some(v))) else {
            continue;
        };
        if let Some(value) = default_value_for_submitter(state, form, value, field, true) {
            defaults.insert(uuid.to_owned(), value);
        }
    }
    defaults.retain(|_, v| present(Some(v)));
    if let Some(values) = form.submitter.values.as_object() {
        defaults.extend(values.clone());
    }
    Ok(Ok(defaults))
}

/// `SubmitValues.template_default_value_for_submitter`.
#[must_use]
pub fn default_value_for_submitter(
    state: &AppState,
    form: &Form,
    value: &Value,
    field: &Value,
    with_time: bool,
) -> Option<Value> {
    default_value_for(state, form, &form.submitter, value, field, with_time)
}

/// The same for any submitter of the form.
#[must_use]
pub fn default_value_for(
    state: &AppState,
    form: &Form,
    submitter: &submissions::Submitter,
    value: &Value,
    field: &Value,
    with_time: bool,
) -> Option<Value> {
    if blank(Some(value)) {
        return None;
    }
    let role = form.role(&submitter.uuid);
    let mut attrs = Map::new();
    // An unsaved submission (the API completing a recipient it creates) has no id yet.
    attrs.insert(
        "submission_id".into(),
        json!((submitter.submission_id != 0).then_some(submitter.submission_id)),
    );
    attrs.insert("email".into(), json!(submitter.email));
    attrs.insert("phone".into(), json!(submitter.phone));
    attrs.insert("name".into(), json!(submitter.name));
    attrs.insert("role".into(), json!(role));

    let account = &form.account;
    let clock = |key: &str| -> Option<String> {
        let now = Utc::now();
        let tz = crate::api::time::account_zone(&account.timezone);
        let local = now.with_timezone(&tz);
        Some(match key {
            "time" => crate::l10n::time(
                state,
                &account.locale,
                now.naive_utc(),
                &account.timezone,
                "long",
            ),
            "hour" => local.format("%-k").to_string().trim().to_owned(),
            "minute" => local.format("%M").to_string(),
            "day" => local.format("%-d").to_string(),
            "month" => local.format("%-m").to_string(),
            "year" => local.format("%Y").to_string(),
            "date" => rules::current_date_value(
                field.pointer("/preferences/format").and_then(Value::as_str),
                now,
                tz,
            ),
            _ => return None,
        })
    };
    rules::replace_default_variables(
        value,
        &attrs,
        with_time.then_some(&clock as &dyn Fn(&str) -> Option<String>),
    )
}

/// `maybe_set_signature_reason!`: a typed reason becomes a read-only text field after the
/// signature field. Returns the reason field's uuid.
async fn maybe_set_signature_reason(
    tx: &mut loco_db::PgConnection,
    form: &mut Form,
    values: &Map<String, Value>,
    params: &Value,
    reason_label: &str,
) -> Result<Result<Option<String>, SubmitError>, AppError> {
    let Some(reason_uuid) = params
        .get("with_reason")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
    else {
        return Ok(Ok(None));
    };
    let signature_uuid = values
        .keys()
        .find(|k| **k != reason_uuid)
        .cloned()
        .unwrap_or_default();
    let mut fields: Vec<Value> = form.fields().to_vec();
    let Some(signature_index) = fields.iter().position(|f| {
        f.get("uuid").and_then(Value::as_str) == Some(signature_uuid.as_str())
            && f.get("submitter_uuid").and_then(Value::as_str) == Some(&form.submitter.uuid)
    }) else {
        return Ok(Err(SubmitError::Invalid("Invalid field".into())));
    };
    if let Some(reason) = fields
        .iter()
        .find(|f| f.get("uuid").and_then(Value::as_str) == Some(reason_uuid.as_str()))
    {
        if reason
            .pointer("/preferences/signature_field_uuid")
            .and_then(Value::as_str)
            != Some(signature_uuid.as_str())
        {
            return Ok(Err(SubmitError::Invalid("Invalid field".into())));
        }
    } else {
        fields.insert(
            signature_index + 1,
            json!({
                "type": "text",
                "uuid": reason_uuid,
                "name": reason_label,
                "readonly": true,
                "preferences": { "signature_field_uuid": signature_uuid },
                "submitter_uuid": form.submitter.uuid,
            }),
        );
    }
    let signature = &mut fields[signature_index];
    if !signature.get("preferences").is_some_and(Value::is_object) {
        signature["preferences"] = json!({});
    }
    signature["preferences"]["reason_field_uuid"] = json!(reason_uuid);
    let fields = JsonText(Value::Array(fields));
    signing::save_template_fields(&mut *tx, form.submission.id, &fields).await?;
    form.submission.template_fields = Some(fields);
    Ok(Ok(Some(reason_uuid)))
}

/// `validate_invite_submitters!`: a party this one must invite has not been invited.
fn pending_invite(form: &Form) -> bool {
    form.template_submitters().iter().any(|s| {
        s.get("invite_by_uuid").and_then(Value::as_str) == Some(&form.submitter.uuid)
            && !form
                .submitters
                .iter()
                .any(|e| Some(e.uuid.as_str()) == s.get("uuid").and_then(Value::as_str))
    })
}

/// `maybe_invite_via_field`: parties whose email or phone comes from this submitter's
/// field are created.
pub(crate) async fn invite_via_field(
    tx: &mut loco_db::PgConnection,
    state: &AppState,
    form: &Form,
    values: Map<String, Value>,
    request: &RequestInfo,
) -> Result<Map<String, Value>, AppError> {
    let mut invited = false;
    for item in form.template_submitters() {
        let Some(field_uuid) = item
            .get("invite_via_field_uuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(field) = form
            .fields()
            .iter()
            .find(|f| f.get("uuid").and_then(Value::as_str) == Some(field_uuid))
        else {
            continue;
        };
        if field.get("submitter_uuid").and_then(Value::as_str) != Some(&form.submitter.uuid) {
            continue;
        }
        let uuid = item.get("uuid").and_then(Value::as_str).unwrap_or_default();
        if form.submitters.iter().any(|s| s.uuid == uuid) {
            continue;
        }
        let Some(value) = values
            .get(field_uuid)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
        else {
            continue;
        };
        let (email, phone) = if value.contains('@') {
            (crate::api::create::email::normalize_email(value), None)
        } else if value
            .chars()
            .any(|c| c.is_ascii_digit() || "+()- \t".contains(c))
        {
            let digits: String = value
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '+')
                .collect();
            (None, Some(digits))
        } else {
            (None, None)
        };
        if email.as_deref().is_none_or(str::is_empty) && phone.as_deref().is_none_or(str::is_empty)
        {
            continue;
        }
        let slug = loco_rails_compat::base58(14);
        signing::insert_invited_submitter(
            &mut *tx,
            form.submission.id,
            form.submitter.account_id,
            uuid,
            email.as_deref(),
            phone.as_deref(),
            &slug,
        )
        .await?;
        let mut data = event_data(request);
        data.insert("uuid".into(), json!(uuid));
        submissions::insert_event(
            &mut *tx,
            &form.submitter,
            "invite_party",
            &JsonText(Value::Object(data)),
        )
        .await?;
        invited = true;
    }
    if invited {
        signing::set_submitters_order(&mut *tx, form.submission.id, "preserved").await?;
    }
    let _ = state;
    Ok(values)
}
