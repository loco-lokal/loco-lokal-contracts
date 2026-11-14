//! `Api::SubmittersController#update`.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
};
use chrono::Utc;
use loco_db::{
    JsonText, signing,
    submissions::{self, SubmitterUpdate},
    templates,
};
use serde_json::{Map, Value};

use super::{
    Invalid, Params, array, blank, cast_string, email, merge, normalize_preferences,
    normalize_submitter_params, phone_digits, ruby_to_s, set_template_fields, str_of,
    unprocessable,
};
use crate::{
    api::{
        permit::{Rule, permit},
        serialize::{self, Load, SubmissionData, SubmitterOpts, Urls},
        submitters::authorized_submitter,
    },
    auth::ApiAuth,
    errors::AppError,
    request::ClientIp,
    signing::{
        Form,
        submit::{RequestInfo, SubmitError, assign_completed_values, invite_via_field},
    },
    state::AppState,
};

const FIELD_RULES: &[(&str, Rule)] = &[
    ("name", Rule::Scalar),
    ("uuid", Rule::Scalar),
    ("default_value", Rule::ScalarOrScalars),
    ("value", Rule::ScalarOrScalars),
    ("required", Rule::Scalar),
    ("readonly", Rule::Scalar),
    ("validation_pattern", Rule::Scalar),
    ("invalid_message", Rule::Scalar),
    ("preferences", Rule::AnyHash),
];

const SUBMITTER_RULES: &[(&str, Rule)] = &[
    ("send_email", Rule::Scalar),
    ("send_sms", Rule::Scalar),
    ("reply_to", Rule::Scalar),
    ("completed_redirect_url", Rule::Scalar),
    ("uuid", Rule::Scalar),
    ("name", Rule::Scalar),
    ("email", Rule::Scalar),
    ("role", Rule::Scalar),
    ("completed", Rule::Scalar),
    ("phone", Rule::Scalar),
    ("application_key", Rule::Scalar),
    ("external_id", Rule::Scalar),
    ("go_to_last", Rule::Scalar),
    ("require_phone_2fa", Rule::Scalar),
    ("require_email_2fa", Rule::Scalar),
    ("metadata", Rule::AnyHash),
    ("values", Rule::AnyHash),
    ("readonly_fields", Rule::Scalars),
    ("message", Rule::Object(super::MESSAGE_RULES)),
    ("fields", Rule::Objects(FIELD_RULES)),
];

/// `PUT /api/submitters/:id`.
pub async fn update(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submitter = match authorized_submitter(&state, &auth, &headers, &id).await? {
        Ok(submitter) => submitter,
        Err(response) => return Ok(response),
    };
    let Some(submission) = submissions::find(&state.db, submitter.submission_id).await? else {
        return Err(AppError::Status(axum::http::StatusCode::NOT_FOUND));
    };
    // `maybe_return_submitter_error`.
    if submitter.completed_at.is_some() || submission.completed_at.is_some() {
        return Ok(unprocessable(
            "Submitter has already completed the submission.",
        ));
    }
    if submitter.declined_at.is_some() {
        return Ok(unprocessable(
            "Submitter has already declined the submission.",
        ));
    }
    let template = match submitter.template_id.or(submission.template_id) {
        Some(id) => templates::find(&state.db, id).await?,
        None => None,
    };

    let nested = params.0.get("submitter").filter(|v| v.is_object());
    let mut attrs: Map<String, Value> = permit(nested.unwrap_or(&params.0), SUBMITTER_RULES);

    let template_submitters: Vec<Value> = submission
        .template_submitters
        .as_deref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let role = template_submitters
        .iter()
        .find(|e| str_of(e, "uuid") == Some(submitter.uuid.as_str()))
        .and_then(|e| e.get("name"))
        .cloned()
        .unwrap_or(Value::Null);
    attrs.insert("role".into(), role);

    // Values match the fields of this recipient in the submission's snapshot.
    let template_fields: Vec<Value> = submission
        .template_fields
        .as_deref()
        .or(template.as_ref().map(|t| &*t.fields))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let role_submitters: Vec<Value> = template.as_ref().map_or_else(
        || template_submitters.clone(),
        |t| array(Some(&t.submitters)).to_vec(),
    );
    let pending = match normalize_submitter_params(
        &mut attrs,
        &template_fields,
        &role_submitters,
        0,
        Some(&submitter.uuid),
    ) {
        Ok(pending) => pending,
        Err(Invalid(message)) => return Ok(unprocessable(&message)),
    };
    let mut new_attachments = Vec::new();
    if !pending.is_empty() {
        let mut values = attrs
            .get("values")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let account_id = template
            .as_ref()
            .map_or(submitter.account_id, |t| t.account_id);
        match super::attachments::resolve(
            &state,
            account_id,
            Some(submitter.id),
            pending,
            &mut values,
        )
        .await?
        {
            Ok(created) => new_attachments = created,
            Err(message) => return Ok(unprocessable(&message)),
        }
        attrs.insert("values".into(), Value::Object(values));
    }

    // `maybe_set_template_fields` with the recipient's uuid, then `assign_submission_fields`.
    let mut fields = submission
        .template_fields
        .as_ref()
        .and_then(|f| f.as_array().cloned());
    let mut schema = submission.template_schema.as_ref().map(|s| s.0.clone());
    let mut variables_schema = submission.variables_schema.as_ref().map(|s| s.0.clone());
    if let Some(template) = &template {
        let variables_present = submission
            .variables
            .as_deref()
            .is_some_and(|v| !blank(Some(v)));
        set_template_fields(
            template,
            &mut fields,
            &mut schema,
            &mut variables_schema,
            &template_submitters,
            std::slice::from_ref(&attrs),
            Some(&submitter.uuid),
            variables_present,
        );
        fields.get_or_insert_with(|| array(Some(&template.fields)).to_vec());
        schema.get_or_insert_with(|| template.schema.0.clone());
    }

    // `assign_submitter_attrs`.
    let values: Map<String, Value> = attrs
        .get("values")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let phone_field_uuid = fields
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|f| {
            str_of(f, "type") == Some("phone")
                && str_of(f, "uuid").is_some_and(|u| !blank(values.get(u)))
        })
        .and_then(|f| str_of(f, "uuid"))
        .map(str::to_owned);

    let mut changes = SubmitterUpdate {
        email: submitter.email.clone(),
        name: submitter.name.clone(),
        phone: submitter.phone.clone(),
        external_id: submitter.external_id.clone(),
        values: submitter.values.clone(),
        metadata: submitter.metadata.clone(),
        preferences: submitter.preferences.clone(),
        sent_at: submitter.sent_at,
        completed_at: submitter.completed_at,
    };
    if attrs.contains_key("email") {
        changes.email = attrs
            .get("email")
            .filter(|e| !blank(Some(e)))
            .and_then(|e| email::normalize_email(&ruby_to_s(e)));
    }
    if attrs.contains_key("name") {
        changes.name = cast_string(attrs.get("name"));
    }
    if attrs.contains_key("phone") {
        changes.phone = Some(phone_digits(
            &attrs.get("phone").map(ruby_to_s).unwrap_or_default(),
        ));
    } else if let Some(value) = phone_field_uuid
        .as_ref()
        .and_then(|u| values.get(u))
        .filter(|v| !blank(Some(v)))
    {
        changes.phone = Some(phone_digits(&ruby_to_s(value)));
    }
    let mut new_values = values.clone();
    if let Some(u) = &phone_field_uuid {
        new_values.shift_remove(u);
    }
    if !new_values.is_empty() {
        let base = changes.values.as_object().cloned().unwrap_or_default();
        changes.values = JsonText(Value::Object(merge(base, &new_values)));
    }
    if let Some(metadata) = attrs.get("metadata") {
        changes.metadata = JsonText(metadata.clone());
    }
    if attrs.contains_key("application_key") {
        changes.external_id = cast_string(attrs.get("application_key"));
    }
    if attrs.contains_key("external_id") {
        changes.external_id = cast_string(attrs.get("external_id"));
    }

    // `assign_preferences`.
    let normalized = normalize_preferences(&state, &auth, &attrs).await?;
    let mut preferences = changes.preferences.as_object().cloned().unwrap_or_default();
    if !values.is_empty() {
        preferences.insert("default_values".into(), Value::Object(values));
    }
    for key in [
        "send_email",
        "send_sms",
        "reply_to",
        "require_phone_2fa",
        "require_email_2fa",
        "go_to_last",
        "completed_redirect_url",
        "email_message_uuid",
    ] {
        if let Some(v) = normalized.get(key) {
            preferences.insert(key.into(), v.clone());
        }
    }
    changes.preferences = JsonText(Value::Object(preferences));

    changes.email = super::strip(changes.email);
    changes.name = super::strip(changes.name);
    changes.phone = super::strip(changes.phone);
    changes.external_id = super::strip(changes.external_id);

    // `maybe_assign_completed_attributes`: `attrs[:completed]` is truthy unless false.
    let completed = attrs
        .get("completed")
        .is_some_and(|v| !v.is_null() && v != &Value::Bool(false));
    let mut form = None;
    if completed {
        let mut completing = Form::for_submitter(&state, submitter.clone()).await?;
        completing.submitter.email.clone_from(&changes.email);
        completing.submitter.name.clone_from(&changes.name);
        completing.submitter.phone.clone_from(&changes.phone);
        completing.submitter.values = changes.values.clone();
        completing.submission.template_fields =
            fields.as_ref().map(|f| JsonText(Value::Array(f.clone())));
        completing.submission.template_schema = schema.as_ref().map(|s| JsonText(s.clone()));
        match assign_completed_values(&state, &completing, None).await? {
            Ok(values) => changes.values = JsonText(Value::Object(values)),
            Err(SubmitError::Invalid(message) | SubmitError::Required(message)) => {
                return Ok(unprocessable(&message));
            }
        }
        changes.completed_at = Some(Utc::now().naive_utc());
        form = Some(completing);
    }

    let mut tx = state.db.begin().await?;
    let mut updated = submissions::update_submitter(&mut *tx, submitter.id, &changes).await?;
    if !new_attachments.is_empty() {
        // Saved before the submitter in Rails; attaching touches it.
        super::attachments::save(&mut tx, &new_attachments, &updated).await?;
        updated = submissions::find_submitter(&mut *tx, submitter.id)
            .await?
            .unwrap_or(updated);
    }
    let fields_json = fields.map(|f| JsonText(Value::Array(f)));
    let schema_json = schema.map(JsonText);
    let variables_json = variables_schema.map(JsonText);
    submissions::update_snapshots(
        &mut *tx,
        submission.id,
        fields_json.as_ref(),
        schema_json.as_ref(),
        variables_json.as_ref(),
    )
    .await?;
    if let Some(mut form) = form {
        // `maybe_invite_via_field` and the `api_complete_form` event.
        form.submitter = updated.clone();
        let request = RequestInfo {
            ip: ip.clone(),
            user_agent: headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            headers: headers.clone(),
            sid: None,
            user_id: None,
            locale: crate::locale::DEFAULT_LOCALE.to_owned(),
        };
        let values = updated.values.as_object().cloned().unwrap_or_default();
        invite_via_field(&mut tx, &state, &form, values, &request).await?;
        let data = crate::api::submitters::tracking_data(
            request.ip.as_deref(),
            request.user_agent.as_deref(),
            None,
            None,
            Map::new(),
        );
        submissions::insert_event(
            &mut *tx,
            &updated,
            "api_complete_form",
            &JsonText(Value::Object(data)),
        )
        .await?;
    }
    tx.commit().await?;

    let send = |key: &str| {
        attrs
            .get(key)
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    };
    if updated.completed_at.is_some() && completed {
        let viewer_uuids: Vec<String> = template_submitters
            .iter()
            .filter(|s| !blank(s.get("is_viewer")))
            .filter_map(|s| str_of(s, "uuid").map(str::to_owned))
            .collect();
        let is_last =
            signing::maybe_update_completed_at(&state.db, updated.submission_id, &viewer_uuids)
                .await?;
        crate::signing::completion::enqueue(&state, updated.id, Some(is_last)).await?;
    } else if send("send_email") || send("send_sms") {
        super::send_signature_requests(&state, &[&updated]).await;
    }
    crate::search::reindex_later(&state, "Submitter", &[updated.id]).await;

    let Some(submission) = submissions::find(&state.db, updated.submission_id).await? else {
        return Err(AppError::Status(axum::http::StatusCode::NOT_FOUND));
    };
    let list = vec![submission];
    let by_submission = [(updated.submission_id, vec![updated.clone()])]
        .into_iter()
        .collect();
    let load = Load {
        events: false,
        documents: true,
        attachments: true,
    };
    let data = SubmissionData::with_submitters(&state, &list, by_submission, load).await?;
    let urls = Urls::new(&state, updated.account_id).await?;
    let include_fields = params.str("include").is_some_and(|i| i.contains("fields"));
    Ok(crate::api::ok(&serialize::submitter_json(
        &urls,
        &data,
        &list[0],
        &updated,
        SubmitterOpts {
            with_urls: true,
            include_fields,
            ..SubmitterOpts::default()
        },
    )))
}
