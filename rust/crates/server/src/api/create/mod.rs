//! Submission creation (`POST /api/submissions`, `/init`, `/emails` and
//! `/api/templates/:id/submissions`) and submitter updates (`PUT /api/submitters/:id`),
//! which share the parameter normalization: `Params::SubmissionCreateValidator`,
//! `Submissions::NormalizeParamUtils`, `Submissions::CreateFromSubmitters`,
//! `Submissions.create_from_emails` and `Submitters::NormalizeValues`.
//!
//! Not ported yet, and answered with a 422 that says so: submitters created or updated
//! as `completed` (the completion job and result documents come with Phase 4),
//! prefilled signature, image and file values, and templates with dynamic documents
//! (Phase 5).

pub(crate) mod email;
mod submitter;
mod validate;
pub(crate) mod values;

use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, Uri},
    response::Response,
};
use chrono::{Days, Months, NaiveDateTime, Utc};
use loco_db::{
    JsonText,
    submissions::{self, NewSubmission, NewSubmitter, Submission, Submitter},
    templates::Template,
};
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};

pub use submitter::update;

use super::{
    Params,
    permit::{Rule, permit},
    serialize::{self, Load, SubmissionData, SubmitterOpts, Urls},
    time::parse_in_zone,
};
use crate::{
    auth::{ApiAuth, Auth},
    errors::AppError,
    state::AppState,
    webhooks::{self, EventRecord},
};
use values::{blank, ruby_to_s};

/// `Submitters::TRUE_VALUES`.
fn true_value(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
        || matches!(value.and_then(Value::as_str), Some("1" | "true"))
}

/// A 422 with the message, as the controllers render `BaseError`s.
struct Invalid(String);

impl From<String> for Invalid {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Invalid {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

fn unprocessable(message: &str) -> Response {
    super::unprocessable(message)
}

/// `ActiveModel::Type::String` for a permitted scalar.
fn cast_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::Bool(true) => Some("t".into()),
        Value::Bool(false) => Some("f".into()),
        other => Some(ruby_to_s(other)),
    }
}

/// `strip_attributes` (on every model): string columns are stripped, blank ones nil.
pub(crate) fn strip(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim_matches(char::is_whitespace).to_owned())
        .filter(|v| !v.is_empty())
}

/// `value.presence` as a string.
fn present_str(value: Option<&Value>) -> Option<String> {
    (!blank(value)).then(|| cast_string(value)).flatten()
}

/// Active Record's datetime cast of a parameter; unparseable values are nil.
fn cast_time(value: Option<&Value>) -> Option<NaiveDateTime> {
    let text = super::params::string(value?)?;
    parse_in_zone(&text, chrono_tz::Tz::UTC).map(|t| t.naive_utc())
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `Templates.build_default_expire_at`.
fn default_expire_at(template: &Template) -> Option<NaiveDateTime> {
    let prefs = &template.preferences;
    let duration = present_str(prefs.get("default_expire_at_duration"))?;
    let now = Utc::now().naive_utc();
    if duration == "specified_date" {
        return present_str(prefs.get("default_expire_at"))
            .and_then(|at| parse_in_zone(&at, chrono_tz::Tz::UTC))
            .map(|t| t.naive_utc());
    }
    let days = |n| now.checked_add_days(Days::new(n));
    let months = |n| now.checked_add_months(Months::new(n));
    match duration.as_str() {
        "one_day" => days(1),
        "two_days" => days(2),
        "three_days" => days(3),
        "four_days" => days(4),
        "five_days" => days(5),
        "six_days" => days(6),
        "seven_days" => days(7),
        "eight_days" => days(8),
        "nine_days" => days(9),
        "ten_days" => days(10),
        "two_weeks" => days(14),
        "three_weeks" => days(21),
        "four_weeks" => days(28),
        "one_month" => months(1),
        "two_months" => months(2),
        "three_months" => months(3),
        _ => None,
    }
}

/// `{ subject:, body: }.to_json` as Active Support encodes it.
fn rails_json(value: &Value) -> String {
    super::rails_json(value)
}

/// `Submitters.normalize_preferences`. Creates the email message a custom subject or
/// body needs.
async fn normalize_preferences(
    state: &AppState,
    auth: &Auth,
    params: &Map<String, Value>,
) -> Result<Map<String, Value>, AppError> {
    let mut preferences = Map::new();

    let message: Map<String, Value> = match params.get("message") {
        Some(Value::Object(m)) if !m.is_empty() => m.clone(),
        _ => ["subject", "body"]
            .iter()
            .filter_map(|k| params.get(*k).map(|v| ((*k).to_owned(), v.clone())))
            .collect(),
    }
    .into_iter()
    .filter(|(_, v)| !blank(Some(v)))
    .collect();

    if !message.is_empty() {
        let subject = present_str(message.get("subject")).unwrap_or_else(|| {
            state
                .views
                .t("en", "you_are_invited_to_sign_a_document", &[])
        });
        let subject = strip(Some(subject)).unwrap_or_default();
        let body = strip(message.get("body").map(ruby_to_s)).unwrap_or_default();
        let sha1 = hex::encode(Sha1::digest(
            rails_json(&json!({ "subject": subject, "body": body })).as_bytes(),
        ));
        let uuid = submissions::find_or_create_email_message(
            &state.db,
            auth.account.id,
            auth.user.id,
            &subject,
            &body,
            &sha1,
        )
        .await?;
        preferences.insert("email_message_uuid".into(), uuid.into());
    }
    for key in [
        "send_email",
        "send_sms",
        "require_phone_2fa",
        "require_email_2fa",
    ] {
        if params.contains_key(key) {
            preferences.insert(key.into(), true_value(params.get(key)).into());
        }
    }
    if let Some(bcc) = params.get("bcc_completed") {
        let value = match bcc {
            Value::Array(items) => {
                let mut seen = Vec::new();
                for item in items.iter().filter(|v| !blank(Some(v))) {
                    let s = ruby_to_s(item);
                    if !seen.contains(&s) {
                        seen.push(s);
                    }
                }
                Value::String(seen.join(", "))
            }
            other => other.clone(),
        };
        preferences.insert("bcc_completed".into(), value);
    }
    for key in ["reply_to", "go_to_last", "completed_redirect_url"] {
        if let Some(value) = params.get(key) {
            preferences.insert(key.into(), value.clone());
        }
    }
    Ok(preferences)
}

/// `hash.merge(other)`: existing keys keep their position.
fn merge(mut base: Map<String, Value>, other: &Map<String, Value>) -> Map<String, Value> {
    for (k, v) in other {
        base.insert(k.clone(), v.clone());
    }
    base
}

// --- Strong parameters of `submissions_params` ---------------------------------------

const MESSAGE_RULES: &[(&str, Rule)] = &[("subject", Rule::Scalar), ("body", Rule::Scalar)];

const FIELD_RULES: &[(&str, Rule)] = &[
    ("name", Rule::Scalar),
    ("uuid", Rule::Scalar),
    ("default_value", Rule::ScalarOrScalars),
    ("value", Rule::ScalarOrScalars),
    ("title", Rule::Scalar),
    ("description", Rule::Scalar),
    ("readonly", Rule::Scalar),
    ("required", Rule::Scalar),
    ("validation_pattern", Rule::Scalar),
    ("invalid_message", Rule::Scalar),
    ("preferences", Rule::AnyHash),
    ("validation", Rule::AnyHash),
];

const SUBMITTER_RULES: &[(&str, Rule)] = &[
    ("send_email", Rule::Scalar),
    ("send_sms", Rule::Scalar),
    ("completed_redirect_url", Rule::Scalar),
    ("uuid", Rule::Scalar),
    ("name", Rule::Scalar),
    ("email", Rule::Scalar),
    ("role", Rule::Scalar),
    ("completed", Rule::Scalar),
    ("phone", Rule::Scalar),
    ("application_key", Rule::Scalar),
    ("external_id", Rule::Scalar),
    ("reply_to", Rule::Scalar),
    ("go_to_last", Rule::Scalar),
    ("require_phone_2fa", Rule::Scalar),
    ("require_email_2fa", Rule::Scalar),
    ("order", Rule::Scalar),
    ("index", Rule::Scalar),
    ("invite_by", Rule::Scalar),
    ("metadata", Rule::AnyHash),
    ("values", Rule::AnyHash),
    ("roles", Rule::Scalars),
    ("readonly_fields", Rule::Scalars),
    ("message", Rule::Object(MESSAGE_RULES)),
    ("fields", Rule::Objects(FIELD_RULES)),
];

const SUBMISSION_RULES: &[(&str, Rule)] = &[
    ("send_email", Rule::Scalar),
    ("send_sms", Rule::Scalar),
    ("bcc_completed", Rule::Scalar),
    ("completed_redirect_url", Rule::Scalar),
    ("reply_to", Rule::Scalar),
    ("go_to_last", Rule::Scalar),
    ("require_phone_2fa", Rule::Scalar),
    ("require_email_2fa", Rule::Scalar),
    ("expire_at", Rule::Scalar),
    ("name", Rule::Scalar),
    ("variables", Rule::AnyHash),
    ("message", Rule::Object(MESSAGE_RULES)),
    ("submitters", Rule::Objects(SUBMITTER_RULES)),
];

/// `submissions_params`: one hash per submission.
fn submissions_params(params: &Value) -> Vec<Map<String, Value>> {
    if params.get("submitters").is_some() {
        return vec![permit(params, SUBMISSION_RULES)];
    }
    let key = if params.get("submission").is_some() {
        "submission"
    } else {
        "submissions"
    };
    let permitted = permit(params, &[(key, Rule::ObjectOrObjects(SUBMISSION_RULES))]);
    match permitted.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_object().cloned())
            .collect(),
        Some(Value::Object(map)) => vec![map.clone()],
        _ => Vec::new(),
    }
}

// --- NormalizeParamUtils ---------------------------------------------------------------

/// `normalize_submitter_params!`: merges `fields[].default_value`/`value` into `values`
/// and normalizes them. `template_submitters` are those the role names refer to.
fn normalize_submitter_params(
    submitter: &mut Map<String, Value>,
    template_fields: &[Value],
    template_submitters: &[Value],
    index: usize,
    for_submitter_uuid: Option<&str>,
) -> Result<(), Invalid> {
    let with_values = !blank(submitter.get("values"));
    let mut defaults: Map<String, Value> = if with_values {
        submitter
            .get("values")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    } else {
        Map::new()
    };
    for field in array(submitter.get("fields")) {
        let key = present_str(field.get("name")).or_else(|| cast_string(field.get("uuid")));
        let key = key.unwrap_or_default();
        if let Some(v) = field.get("default_value") {
            defaults.insert(key.clone(), v.clone());
        }
        if let Some(v) = field.get("value") {
            defaults.insert(key, v.clone());
        }
    }
    if defaults.is_empty() {
        return Ok(());
    }

    let role = cast_string(submitter.get("role"));
    let by_uuid = present_str(submitter.get("uuid")).and_then(|uuid| {
        template_submitters
            .iter()
            .find(|s| str_of(s, "uuid") == Some(uuid.as_str()))
            .and_then(|s| str_of(s, "name"))
            .map(str::to_owned)
    });
    let submitter_name = role.or(by_uuid).or_else(|| {
        template_submitters
            .get(index)
            .and_then(|s| str_of(s, "name"))
            .map(str::to_owned)
    });
    let roles: Option<Vec<String>> = submitter
        .get("roles")
        .and_then(Value::as_array)
        .map(|r| r.iter().map(ruby_to_s).collect());

    let normalized = values::normalize(
        &values::Target {
            template_fields,
            template_submitters,
            submitter_name: submitter_name.as_deref(),
            role_names: roles.as_deref(),
            for_submitter_uuid,
        },
        &defaults,
        !with_values,
    )?;
    submitter.insert("values".into(), Value::Object(normalized));
    Ok(())
}

// --- CreateFromSubmitters ----------------------------------------------------------------

/// A submission being built, before it is saved.
struct Draft {
    name: Option<String>,
    preferences: Map<String, Value>,
    variables: Value,
    expire_at: Option<NaiveDateTime>,
    template_submitters: Vec<Value>,
    template_fields: Option<Vec<Value>>,
    template_schema: Option<Value>,
    variables_schema: Option<Value>,
    submitters: Vec<DraftSubmitter>,
}

struct DraftSubmitter {
    uuid: String,
    email: Option<String>,
    /// `None` for recipients built without a phone (`create_from_emails`).
    phone: Option<String>,
    name: Option<String>,
    external_id: Option<String>,
    values: Map<String, Value>,
    metadata: Value,
    preferences: Map<String, Value>,
    sent: bool,
}

/// `find_submitter_uuid`.
fn find_submitter_uuid(
    submitters: &[Value],
    attrs: &Map<String, Value>,
    index: usize,
) -> Option<String> {
    if let Some(uuid) = present_str(attrs.get("uuid")) {
        return Some(uuid);
    }
    let role = attrs.get("role").map(ruby_to_s).unwrap_or_default();
    if let Some(uuid) = submitters
        .iter()
        .find(|e| {
            e.get("name")
                .map(ruby_to_s)
                .unwrap_or_default()
                .eq_ignore_ascii_case(&role)
        })
        .and_then(|e| str_of(e, "uuid"))
    {
        return Some(uuid.to_owned());
    }
    let index = submitter_index(attrs).unwrap_or(index);
    submitters
        .get(index)
        .and_then(|s| str_of(s, "uuid"))
        .map(str::to_owned)
}

/// `submitter_attrs[:index]`.
fn submitter_index(attrs: &Map<String, Value>) -> Option<usize> {
    match attrs.get("index")? {
        Value::Number(n) => n.as_u64().and_then(|n| usize::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn order_of(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

const NON_DEFAULT_TYPES: &[&str] = &["signature", "image", "initials", "file"];

/// `process_readonly_fields_param`.
fn process_readonly_fields(readonly: &[Value], fields: &mut [Value], submitter_uuid: Option<&str>) {
    if readonly.is_empty() {
        return;
    }
    let names: Vec<String> = readonly.iter().map(ruby_to_s).collect();
    for f in fields.iter_mut() {
        if str_of(f, "submitter_uuid") != submitter_uuid {
            continue;
        }
        let name = f.get("name").map(ruby_to_s).unwrap_or_default();
        let matches = f
            .get("name")
            .is_some_and(|n| names.contains(&ruby_to_s(n)) && !n.is_null())
            || names.contains(&name.to_lowercase())
            || names.contains(&values::parameterize_underscore(&name));
        if matches && let Some(map) = f.as_object_mut() {
            map.insert("readonly".into(), true.into());
        }
    }
}

/// `process_field_values_param`.
fn process_field_values(
    values: Option<&Value>,
    fields: &mut [Value],
    submitter_uuid: Option<&str>,
) {
    let Some(values) = values.and_then(Value::as_object).filter(|v| !v.is_empty()) else {
        return;
    };
    for f in fields.iter_mut() {
        if NON_DEFAULT_TYPES.contains(&str_of(f, "type").unwrap_or_default())
            || str_of(f, "submitter_uuid") != submitter_uuid
        {
            continue;
        }
        let Some(uuid) = str_of(f, "uuid").map(str::to_owned) else {
            continue;
        };
        let Some(value) = values.get(&uuid) else {
            continue;
        };
        let Some(map) = f.as_object_mut() else {
            continue;
        };
        if blank(Some(value)) {
            map.shift_remove("default_value");
        } else {
            map.insert("default_value".into(), value.clone());
        }
    }
}

/// `assign_field_attrs`.
fn assign_field_attrs(field: &mut Value, attrs: &Value) {
    let field_type = str_of(field, "type").unwrap_or_default().to_owned();
    let normalized_default = attrs
        .get("default_value")
        .filter(|v| !blank(Some(v)))
        .map(|v| values::normalize_value(field, v));
    let Some(map) = field.as_object_mut() else {
        return;
    };
    for key in ["title", "description"] {
        if let Some(v) = attrs.get(key).filter(|v| !blank(Some(v))) {
            map.insert(key.into(), v.clone());
        }
    }
    for key in ["readonly", "required"] {
        if let Some(v) = attrs.get(key) {
            map.insert(key.into(), v.clone());
        }
    }
    if attrs.get("default_value").is_some() && !NON_DEFAULT_TYPES.contains(&field_type.as_str()) {
        match normalized_default {
            Some(v) => {
                map.insert("default_value".into(), v);
            }
            None => {
                map.shift_remove("default_value");
            }
        }
    }
    for key in ["preferences", "validation"] {
        if let Some(Value::Object(extra)) = attrs.get(key).filter(|v| !blank(Some(v))) {
            let base = map
                .get(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            map.insert(key.into(), Value::Object(merge(base, extra)));
        }
    }
    if let Some(pattern) = attrs.get("validation_pattern").filter(|v| !blank(Some(v))) {
        let mut validation = Map::new();
        validation.insert("pattern".into(), pattern.clone());
        if let Some(message) = attrs.get("invalid_message").filter(|v| !blank(Some(v))) {
            validation.insert("message".into(), message.clone());
        }
        map.insert("validation".into(), Value::Object(validation));
    }
}

/// `process_fields_param`.
fn process_fields(fields_param: &[Value], fields: &mut [Value], submitter_uuid: Option<&str>) {
    if fields_param.is_empty() {
        return;
    }
    for f in fields.iter_mut() {
        if str_of(f, "submitter_uuid") != submitter_uuid {
            continue;
        }
        let name = f.get("name").map(ruby_to_s).unwrap_or_default();
        let config = fields_param.iter().find(|e| {
            if blank(e.get("name")) {
                e.get("uuid") == f.get("uuid")
            } else {
                let n = ruby_to_s(&e["name"]);
                n.eq_ignore_ascii_case(&name) || n == values::parameterize_underscore(&name)
            }
        });
        if let Some(config) = config.filter(|c| !blank(Some(c))) {
            assign_field_attrs(f, config);
        }
    }
}

/// `maybe_set_template_fields`.
#[allow(clippy::too_many_arguments)] // the snapshots it updates and the Ruby method's inputs
fn set_template_fields(
    template: &Template,
    template_fields: &mut Option<Vec<Value>>,
    template_schema: &mut Option<Value>,
    variables_schema: &mut Option<Value>,
    template_submitters: &[Value],
    submitters_attrs: &[Map<String, Value>],
    default_submitter_uuid: Option<&str>,
    variables_present: bool,
) {
    let original: Vec<Value> = template_fields
        .clone()
        .unwrap_or_else(|| array(Some(&template.fields)).to_vec());
    let mut fields = original.clone();
    for (index, attrs) in submitters_attrs.iter().enumerate() {
        let uuid = default_submitter_uuid
            .map(str::to_owned)
            .or_else(|| find_submitter_uuid(template_submitters, attrs, index));
        process_readonly_fields(
            array(attrs.get("readonly_fields")),
            &mut fields,
            uuid.as_deref(),
        );
        process_field_values(attrs.get("values"), &mut fields, uuid.as_deref());
        process_fields(array(attrs.get("fields")), &mut fields, uuid.as_deref());
    }
    let template_variables = template
        .variables_schema
        .as_deref()
        .is_some_and(|v| !blank(Some(v)));
    if fields != original
        || submitters_attrs.iter().any(|e| !blank(e.get("completed")))
        || variables_present
        || template_variables
    {
        *template_fields = Some(fields);
        if template_schema.as_ref().is_none_or(|s| blank(Some(s))) {
            *template_schema = Some(template.schema.0.clone());
        }
        if variables_schema.as_ref().is_none_or(|s| blank(Some(s))) {
            *variables_schema = template.variables_schema.as_ref().map(|v| v.0.clone());
        }
    }
}

/// `merge_submitters_and_fields`: one recipient for several roles.
fn merge_submitters_and_fields(
    attrs: &Map<String, Value>,
    template_submitters: &mut Vec<Value>,
    template_fields: &mut Vec<Value>,
) -> Result<Value, Invalid> {
    let roles: Vec<String> = array(attrs.get("roles")).iter().map(ruby_to_s).collect();
    let mut selected = Vec::new();
    for role in &roles {
        let found = template_submitters
            .iter()
            .find(|e| {
                e.get("name")
                    .map(ruby_to_s)
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(role)
            })
            .cloned()
            .ok_or_else(|| Invalid(format!("{role} role doesn't exist")))?;
        selected.push(found);
    }
    let merge_uuids: Vec<String> = selected
        .iter()
        .filter_map(|s| str_of(s, "uuid"))
        .map(str::to_owned)
        .collect();
    let old_uuids: Vec<String> = template_submitters
        .iter()
        .filter_map(|s| str_of(s, "uuid"))
        .map(str::to_owned)
        .collect();
    let name = present_str(attrs.get("role")).unwrap_or_else(|| {
        selected
            .iter()
            .map(|s| s.get("name").map(ruby_to_s).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" / ")
    });

    // `build_merged_submitter`.
    let mut sorted = merge_uuids.clone();
    sorted.sort();
    let new_uuid =
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, sorted.join(":").as_bytes()).to_string();
    let mut merged: Option<Value> = None;
    let mut next = Vec::new();
    for mut submitter in template_submitters.drain(..) {
        if let Some(map) = submitter.as_object_mut() {
            for key in [
                "optional_invite_by_uuid",
                "invite_by_uuid",
                "linked_to_uuid",
            ] {
                if map
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|u| merge_uuids.iter().any(|m| m == u))
                {
                    map.insert(key.into(), new_uuid.clone().into());
                }
            }
        }
        if str_of(&submitter, "uuid").is_some_and(|u| merge_uuids.iter().any(|m| m == u)) {
            if merged.is_some() {
                continue;
            }
            let mut copy = submitter.clone();
            if let Some(map) = copy.as_object_mut() {
                map.insert("uuid".into(), new_uuid.clone().into());
                map.insert("name".into(), name.clone().into());
                map.shift_remove("linked_to_uuid");
            }
            merged = Some(copy.clone());
            next.push(copy);
        } else {
            next.push(submitter);
        }
    }
    *template_submitters = next;

    // Fields of the merged roles move to the new recipient; same-named ones are joined.
    let position = |f: &Value| {
        str_of(f, "submitter_uuid")
            .and_then(|u| old_uuids.iter().position(|o| o == u))
            .unwrap_or(usize::MAX)
    };
    let mut order: Vec<usize> = (0..template_fields.len()).collect();
    order.sort_by_key(|&i| position(&template_fields[i]));
    let mut by_name: Vec<(String, usize)> = Vec::new();
    let mut removed = HashSet::new();
    for i in order {
        let field = &template_fields[i];
        if !str_of(field, "submitter_uuid").is_some_and(|u| merge_uuids.iter().any(|m| m == u)) {
            continue;
        }
        let field_name = field.get("name").map(ruby_to_s).unwrap_or_default();
        if let Some(&(_, existing)) = by_name.iter().find(|(n, _)| *n == field_name) {
            let areas = array(field.get("areas")).to_vec();
            if let Some(map) = template_fields[existing].as_object_mut() {
                let list = map.entry("areas").or_insert_with(|| Value::Array(vec![]));
                if let Value::Array(list) = list {
                    list.extend(areas);
                } else {
                    *list = Value::Array(areas);
                }
            }
            removed.insert(i);
        } else {
            if let Some(map) = template_fields[i].as_object_mut() {
                map.insert("submitter_uuid".into(), new_uuid.clone().into());
            }
            if !field_name.trim().is_empty() {
                by_name.push((field_name, i));
            }
        }
    }
    let mut index = 0;
    template_fields.retain(|_| {
        let keep = !removed.contains(&index);
        index += 1;
        keep
    });
    merged.ok_or_else(|| "Invalid submitter params".into())
}

fn phone_digits(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

/// `build_submitter`.
#[allow(clippy::too_many_arguments)] // the Ruby method's arguments
async fn build_submitter(
    state: &AppState,
    auth: &Auth,
    draft: &mut Draft,
    template: &Template,
    attrs: &Map<String, Value>,
    uuid: String,
    is_order_sent: bool,
    preferences: &Map<String, Value>,
) -> Result<(), AppError> {
    let email = attrs
        .get("email")
        .filter(|e| !blank(Some(e)))
        .and_then(|e| email::normalize_email(&ruby_to_s(e)));
    let submitter_preferences = normalize_preferences(state, auth, attrs).await?;
    let values: Map<String, Value> = attrs
        .get("values")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let fields = draft
        .template_fields
        .as_deref()
        .unwrap_or_else(|| array(Some(&template.fields)));
    let phone_field_uuid = fields
        .iter()
        .find(|f| {
            str_of(f, "type") == Some("phone")
                && str_of(f, "uuid").is_some_and(|u| !blank(values.get(u)))
        })
        .and_then(|f| str_of(f, "uuid"))
        .map(str::to_owned);
    let phone_value = attrs
        .get("phone")
        .filter(|p| !p.is_null())
        .or_else(|| phone_field_uuid.as_ref().and_then(|u| values.get(u)));
    let phone = phone_digits(&phone_value.map(ruby_to_s).unwrap_or_default());
    let mut stored_values = values.clone();
    if let Some(u) = &phone_field_uuid {
        stored_values.shift_remove(u);
    }

    let mut prefs = merge(preferences.clone(), &submitter_preferences);
    if !values.is_empty() {
        prefs.insert("default_values".into(), Value::Object(values));
    }
    prefs.shift_remove("bcc_completed");
    let send =
        prefs.get("send_email") != Some(&Value::Bool(false)) && email.is_some() && is_order_sent;

    draft.submitters.push(DraftSubmitter {
        uuid,
        email,
        phone: Some(phone),
        name: cast_string(attrs.get("name")),
        external_id: present_str(attrs.get("external_id"))
            .or_else(|| cast_string(attrs.get("application_key"))),
        values: stored_values,
        metadata: attrs
            .get("metadata")
            .filter(|m| !m.is_null())
            .cloned()
            .unwrap_or_else(|| json!({})),
        preferences: prefs,
        sent: send,
    });
    Ok(())
}

/// `maybe_add_invite_submitters`.
fn add_invite_submitters(draft: &mut Draft, template: &Template, attrs: &[Map<String, Value>]) {
    let template_submitters = array(Some(&template.submitters));
    for (index, item) in template_submitters.iter().enumerate() {
        let item_name = item.get("name").map(ruby_to_s).unwrap_or_default();
        let attr = attrs.iter().find(|e| {
            e.get("role")
                .map(ruby_to_s)
                .unwrap_or_default()
                .to_lowercase()
                == item_name.to_lowercase()
        });
        let mut item = item.clone();
        if let Some(attr) = attr
            && let Some(invite_by) = present_str(attr.get("invite_by"))
            && let Some(uuid) = template_submitters
                .iter()
                .find(|s| str_of(s, "name") == Some(invite_by.as_str()))
                .and_then(|s| s.get("uuid"))
            && let Some(map) = item.as_object_mut()
        {
            map.insert("invite_by_uuid".into(), uuid.clone());
        }
        if [
            "invite_by_uuid",
            "optional_invite_by_uuid",
            "invite_via_field_uuid",
        ]
        .iter()
        .all(|k| blank(item.get(*k)))
        {
            continue;
        }
        if draft
            .template_submitters
            .iter()
            .any(|e| e.get("uuid") == item.get("uuid"))
        {
            continue;
        }
        if let Some(order) = attr
            .and_then(|a| a.get("order"))
            .filter(|o| !blank(Some(o)))
            && let Some(map) = item.as_object_mut()
        {
            map.insert("order".into(), order.clone());
        }
        if index == 0 {
            let at = draft.template_submitters.len().min(1);
            draft.template_submitters.insert(at, item);
        } else if draft.template_submitters.len() > index {
            draft.template_submitters.insert(index, item);
        } else {
            draft.template_submitters.push(item);
        }
    }
}

/// `maybe_add_cc_submitters`.
async fn add_cc_submitters(
    state: &AppState,
    auth: &Auth,
    draft: &mut Draft,
    template: &Template,
    recipients: Option<&Value>,
    random_order: bool,
    preferences: &Map<String, Value>,
) -> Result<(), AppError> {
    let order = draft
        .template_submitters
        .iter()
        .filter_map(|s| order_of(s.get("order")))
        .max();
    let recipients: Vec<Value> = match recipients {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    };
    let mut cc_preferences = preferences.clone();
    cc_preferences.shift_remove("email_message_uuid");
    for recipient in recipients {
        let attrs: Map<String, Value> = match &recipient {
            Value::String(s) => [("email".to_owned(), Value::String(s.clone()))]
                .into_iter()
                .collect(),
            Value::Object(map) => ["name", "email", "phone"]
                .iter()
                .filter_map(|k| map.get(*k).map(|v| ((*k).to_owned(), v.clone())))
                .collect(),
            _ => continue,
        };
        if attrs.values().all(|v| blank(Some(v))) {
            continue;
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let mut item = json!({ "name": "CC", "uuid": uuid, "is_viewer": true });
        if let Some(order) = order {
            item["order"] = order.into();
        }
        draft.template_submitters.push(item);
        build_submitter(
            state,
            auth,
            draft,
            template,
            &attrs,
            uuid,
            random_order,
            &cc_preferences,
        )
        .await?;
    }
    Ok(())
}

/// `Submissions.find_first_viewers` over the draft or saved recipients.
fn first_viewers<'a>(
    template_submitters: &[Value],
    first_uuid: &str,
    pending: &'a [(String, bool)],
) -> Vec<&'a str> {
    let mut viewers = Vec::new();
    for s in template_submitters {
        let Some(uuid) = str_of(s, "uuid") else {
            continue;
        };
        let Some((u, viewer)) = pending.iter().find(|(u, _)| u == uuid) else {
            continue;
        };
        if !viewer && u != first_uuid {
            break;
        }
        if *viewer {
            viewers.push(u.as_str());
        }
    }
    viewers
}

/// `assign_submitters_is_viewer`.
fn assign_submitters_is_viewer(draft: &mut Draft, template: &Template) {
    let fields = draft
        .template_fields
        .as_deref()
        .unwrap_or_else(|| array(Some(&template.fields)));
    let field_uuids: HashSet<String> = fields
        .iter()
        .filter_map(|f| f.get("submitter_uuid").map(ruby_to_s))
        .collect();
    let mut any_viewer = false;
    for s in &mut draft.template_submitters {
        if !str_of(s, "uuid").is_some_and(|u| field_uuids.contains(u))
            && let Some(map) = s.as_object_mut()
        {
            map.insert("is_viewer".into(), true.into());
            any_viewer = true;
        }
    }
    if !any_viewer
        || draft
            .template_submitters
            .iter()
            .any(|s| !blank(s.get("order")))
    {
        return;
    }
    let Some(first) = draft
        .submitters
        .iter()
        .find(|s| s.sent)
        .map(|s| s.uuid.clone())
    else {
        return;
    };
    let pending = viewer_flags(
        &draft.template_submitters,
        draft.submitters.iter().map(|s| s.uuid.as_str()),
    );
    let viewers: Vec<String> = first_viewers(&draft.template_submitters, &first, &pending)
        .into_iter()
        .map(str::to_owned)
        .collect();
    for s in &mut draft.submitters {
        if viewers.contains(&s.uuid)
            && s.preferences.get("send_email") != Some(&Value::Bool(false))
            && s.email.is_some()
        {
            s.sent = true;
        }
    }
}

/// `(uuid, viewer?)` of the recipients, in their order, the first per uuid.
fn viewer_flags<'a>(
    template_submitters: &[Value],
    uuids: impl Iterator<Item = &'a str>,
) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for uuid in uuids {
        if out.iter().any(|(u, _)| u == uuid) {
            continue;
        }
        let viewer = template_submitters.iter().any(|s| {
            str_of(s, "uuid") == Some(uuid) && s.get("is_viewer") == Some(&Value::Bool(true))
        });
        out.push((uuid.to_owned(), viewer));
    }
    out
}

/// Rejects what the Rust server cannot do yet.
fn unsupported(template: &Template, submitters: &[Map<String, Value>]) -> Result<(), Invalid> {
    if submitters.iter().any(|s| !blank(s.get("completed"))) {
        return Err("Creating completed submitters is not supported yet".into());
    }
    let dynamic = array(Some(&template.schema))
        .iter()
        .any(|e| !blank(e.get("dynamic")));
    if dynamic
        && template
            .variables_schema
            .as_deref()
            .is_none_or(|v| blank(Some(v)))
    {
        return Err("Templates with dynamic documents are not supported yet".into());
    }
    Ok(())
}

/// `CreateFromSubmitters.call` (`with_template: true`), up to saving.
async fn build_from_submitters(
    state: &AppState,
    auth: &Auth,
    template: &Template,
    params: &Value,
    submissions_attrs: Vec<Map<String, Value>>,
    submitters_order: &str,
) -> Result<Result<Vec<Draft>, Invalid>, AppError> {
    let params_map = params.as_object().cloned().unwrap_or_default();
    let preferences = normalize_preferences(state, auth, &params_map).await?;
    let mut drafts = Vec::new();

    for attrs in submissions_attrs {
        let given_submitters: Vec<Map<String, Value>> = array(attrs.get("submitters"))
            .iter()
            .filter_map(|v| v.as_object().cloned())
            .collect();
        if let Err(e) = unsupported(template, &given_submitters) {
            return Ok(Err(e));
        }
        let submission_preferences = merge(
            preferences.clone(),
            &normalize_preferences(state, auth, &attrs).await?,
        );
        let mut set_preferences = Map::new();
        for key in ["send_email", "bcc_completed"] {
            if let Some(v) = submission_preferences.get(key) {
                set_preferences.insert(key.into(), v.clone());
            }
        }
        if params
            .get("send_completed_email")
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        {
            set_preferences.insert("send_email".into(), true.into());
        }
        let expire_at = match attrs.get("expire_at") {
            Some(v) if !v.is_null() && v != &Value::Bool(false) => cast_time(Some(v)),
            _ => default_expire_at(template),
        };
        let mut draft = Draft {
            name: cast_string(attrs.get("name")),
            preferences: set_preferences,
            variables: attrs
                .get("variables")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!({})),
            expire_at,
            template_submitters: Vec::new(),
            template_fields: None,
            template_schema: None,
            variables_schema: None,
            submitters: Vec::new(),
        };
        let mut template_submitters: Vec<Value> = array(Some(&template.submitters)).to_vec();
        let submitter_preferences = merge(preferences.clone(), &submission_preferences);

        for (index, given) in given_submitters.iter().enumerate() {
            let mut submitter_attrs = given.clone();
            let roles = array(submitter_attrs.get("roles")).to_vec();
            let (uuid, template_submitter) = if roles.len() > 1 {
                let mut fields = draft
                    .template_fields
                    .take()
                    .unwrap_or_else(|| array(Some(&template.fields)).to_vec());
                let merged = merge_submitters_and_fields(
                    &submitter_attrs,
                    &mut template_submitters,
                    &mut fields,
                );
                draft.template_fields = Some(fields);
                let merged = match merged {
                    Ok(m) => m,
                    Err(e) => return Ok(Err(e)),
                };
                if draft
                    .template_schema
                    .as_ref()
                    .is_none_or(|s| blank(Some(s)))
                {
                    draft.template_schema = Some(template.schema.0.clone());
                }
                (
                    str_of(&merged, "uuid").unwrap_or_default().to_owned(),
                    Some(merged),
                )
            } else {
                if let [role] = roles.as_slice() {
                    submitter_attrs.insert("role".into(), role.clone());
                }
                let Some(uuid) = find_submitter_uuid(&template_submitters, &submitter_attrs, index)
                else {
                    continue;
                };
                if ["email", "phone", "name"]
                    .iter()
                    .all(|k| blank(submitter_attrs.get(*k)))
                {
                    continue;
                }
                let found = template_submitters
                    .iter()
                    .find(|e| str_of(e, "uuid") == Some(uuid.as_str()))
                    .cloned();
                (uuid, found)
            };
            let Some(mut template_submitter) = template_submitter else {
                return Ok(Err("Invalid submitter params".into()));
            };
            if let Some(map) = template_submitter.as_object_mut() {
                for key in [
                    "optional_invite_by_uuid",
                    "invite_by_uuid",
                    "invite_via_field_uuid",
                ] {
                    map.shift_remove(key);
                }
                if let Some(order) = submitter_attrs.get("order").filter(|o| !blank(Some(o))) {
                    map.insert("order".into(), order.clone());
                }
            }
            let position = order_of(template_submitter.get("order"))
                .or_else(|| submitter_index(&submitter_attrs).and_then(|i| i64::try_from(i).ok()))
                .unwrap_or_else(|| i64::try_from(index).unwrap_or(i64::MAX));
            let is_order_sent = submitters_order == "random" || position == 0;
            draft.template_submitters.push(template_submitter);
            build_submitter(
                state,
                auth,
                &mut draft,
                template,
                &submitter_attrs,
                uuid,
                is_order_sent,
                &submitter_preferences,
            )
            .await?;
        }

        let variables_present = !blank(Some(&draft.variables));
        let template_submitters_snapshot = draft.template_submitters.clone();
        set_template_fields(
            template,
            &mut draft.template_fields,
            &mut draft.template_schema,
            &mut draft.variables_schema,
            &template_submitters_snapshot,
            &given_submitters,
            None,
            variables_present,
        );

        if draft.submitters.len() > array(Some(&template.submitters)).len() {
            return Ok(Err("Defined more signing parties than in template".into()));
        }
        if template.preferences.get("validate_unique_submitters") == Some(&Value::Bool(true)) {
            let emails: Vec<&String> = draft
                .submitters
                .iter()
                .filter_map(|s| s.email.as_ref())
                .collect();
            let unique: HashSet<&&String> = emails.iter().collect();
            if unique.len() != emails.len() {
                return Ok(Err("Recipient emails should differ".into()));
            }
        }
        if draft.submitters.is_empty() {
            continue;
        }
        add_invite_submitters(&mut draft, template, &given_submitters);
        add_cc_submitters(
            state,
            auth,
            &mut draft,
            template,
            params.get("cc_recipients"),
            submitters_order == "random",
            &submitter_preferences,
        )
        .await?;
        assign_submitters_is_viewer(&mut draft, template);
        drafts.push(draft);
    }
    Ok(Ok(drafts))
}

/// `Submissions.create_from_emails`, up to saving.
async fn build_from_emails(
    state: &AppState,
    auth: &Auth,
    template: &Template,
    params: &Value,
    emails: &Value,
    mark_as_sent: bool,
) -> Result<Result<Vec<Draft>, Invalid>, AppError> {
    if let Err(e) = unsupported(template, &[]) {
        return Ok(Err(e));
    }
    let params_map = params.as_object().cloned().unwrap_or_default();
    let preferences = normalize_preferences(state, auth, &params_map).await?;
    let expire_at = match params.get("expire_at").filter(|v| !blank(Some(v))) {
        Some(v) => cast_time(Some(v)),
        None => default_expire_at(template),
    };
    let list: Vec<String> = match emails {
        Value::Array(items) => items.iter().map(ruby_to_s).collect(),
        other => email::parse_emails(&ruby_to_s(other)),
    };
    let mut seen = HashSet::new();
    let template_submitters = array(Some(&template.submitters)).to_vec();
    let first_uuid = template_submitters
        .first()
        .and_then(|s| str_of(s, "uuid"))
        .unwrap_or_default()
        .to_owned();

    let mut drafts = Vec::new();
    for address in list {
        if !seen.insert(address.clone()) {
            continue;
        }
        let mut draft = Draft {
            name: None,
            preferences: Map::new(),
            variables: Value::Null,
            expire_at,
            template_submitters: template_submitters.clone(),
            template_fields: None,
            template_schema: None,
            variables_schema: None,
            submitters: vec![DraftSubmitter {
                uuid: first_uuid.clone(),
                email: email::normalize_email(&address),
                phone: None,
                name: None,
                external_id: None,
                values: Map::new(),
                metadata: json!({}),
                preferences: preferences.clone(),
                sent: mark_as_sent,
            }],
        };
        assign_submitters_is_viewer(&mut draft, template);
        drafts.push(draft);
    }
    Ok(Ok(drafts))
}

/// Saves the drafts: `submission.save!` with its submitters.
async fn save(
    state: &AppState,
    auth: &Auth,
    template: &Template,
    drafts: Vec<Draft>,
    source: &str,
    submitters_order: &str,
) -> Result<Vec<(Submission, Vec<Submitter>)>, AppError> {
    let now = Utc::now().naive_utc();
    let mut tx = state.db.begin().await?;
    let mut saved = Vec::new();
    for draft in drafts {
        let submission = submissions::insert(
            &mut *tx,
            &NewSubmission {
                template_id: Some(template.id),
                created_by_user_id: Some(auth.user.id),
                account_id: auth.account.id,
                slug: loco_rails_compat::base58(14),
                source: source.to_owned(),
                submitters_order: submitters_order.to_owned(),
                name: strip(draft.name),
                preferences: JsonText(Value::Object(draft.preferences)),
                variables: (!draft.variables.is_null()).then(|| JsonText(draft.variables)),
                expire_at: draft.expire_at,
                template_fields: draft.template_fields.map(|f| JsonText(Value::Array(f))),
                template_schema: draft.template_schema.map(JsonText),
                template_submitters: Some(JsonText(Value::Array(draft.template_submitters))),
                variables_schema: draft.variables_schema.map(JsonText),
            },
        )
        .await?;
        let mut submitters = Vec::new();
        for s in draft.submitters {
            submitters.push(
                submissions::insert_submitter(
                    &mut *tx,
                    &NewSubmitter {
                        submission_id: submission.id,
                        template_id: submission.template_id,
                        account_id: auth.account.id,
                        uuid: s.uuid,
                        slug: loco_rails_compat::base58(14),
                        email: strip(s.email),
                        phone: strip(s.phone),
                        name: strip(s.name),
                        external_id: strip(s.external_id),
                        values: JsonText(Value::Object(s.values)),
                        metadata: JsonText(s.metadata),
                        preferences: JsonText(Value::Object(s.preferences)),
                        sent_at: s.sent.then_some(now),
                    },
                )
                .await?,
            );
        }
        saved.push((submission, submitters));
    }
    tx.commit().await?;
    for (submission, _) in &saved {
        if submission.expire_at.is_some() {
            // `ProcessSubmissionExpiredJob` comes with the expiration jobs (Phase 7).
            tracing::debug!(submission_id = submission.id, "submission expires");
        }
    }
    Ok(saved)
}

/// `Submitters.send_signature_requests`.
pub(crate) async fn send_signature_requests(state: &AppState, submitters: &[&Submitter]) {
    for submitter in submitters {
        if submitter
            .email
            .as_deref()
            .is_none_or(|e| e.trim().is_empty())
            || submitter.declined_at.is_some()
            || submitter.preferences.get("send_email") == Some(&Value::Bool(false))
        {
            continue;
        }
        crate::mail::invitation_later(state, submitter.id).await;
    }
}

/// `Submissions.send_signature_requests`: the first recipients in order get invitations.
async fn send_submission_requests(state: &AppState, saved: &[(Submission, Vec<Submitter>)]) {
    for (submission, submitters) in saved {
        let template_submitters = submission
            .template_submitters
            .as_deref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pending: Vec<&Submitter> = {
            let mut seen = HashSet::new();
            submitters
                .iter()
                .filter(|s| s.completed_at.is_none())
                .rev()
                .filter(|s| seen.insert(s.uuid.clone()))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };
        let find = |uuid: &str| pending.iter().find(|s| s.uuid == uuid).copied();
        let is_viewer = |s: &Submitter| {
            template_submitters.iter().any(|t| {
                str_of(t, "uuid") == Some(s.uuid.as_str())
                    && t.get("is_viewer") == Some(&Value::Bool(true))
            })
        };
        if template_submitters.iter().any(|s| !blank(s.get("order"))) {
            let min = template_submitters
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    order_of(s.get("order")).unwrap_or_else(|| i64::try_from(i).unwrap_or(i64::MAX))
                })
                .min();
            let first: Vec<&Submitter> = template_submitters
                .iter()
                .filter(|s| order_of(s.get("order")) == min && s.get("order").is_some())
                .filter_map(|s| str_of(s, "uuid").and_then(find))
                .collect();
            send_signature_requests(state, &first).await;
        } else if submission.submitters_order == "preserved" {
            let first = template_submitters
                .iter()
                .filter_map(|s| str_of(s, "uuid").and_then(find))
                .find(|s| !is_viewer(s));
            if let Some(first) = first {
                let flags: Vec<(String, bool)> = pending
                    .iter()
                    .map(|s| (s.uuid.clone(), is_viewer(s)))
                    .collect();
                let mut list = vec![first];
                list.extend(
                    first_viewers(&template_submitters, &first.uuid, &flags)
                        .into_iter()
                        .filter_map(find),
                );
                send_signature_requests(state, &list).await;
            } else if submitters.iter().all(is_viewer) {
                send_signature_requests(state, &pending).await;
            }
        } else {
            send_signature_requests(state, &pending).await;
        }
    }
}

/// `Api::SubmissionsController#create` for the template.
async fn create_submissions(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    uri: &Uri,
    template_id: Option<String>,
    mut params: Value,
) -> Result<Response, AppError> {
    // `load_and_authorize_resource :template` loads the parent by `template_id`; the
    // path parameter is part of `params`.
    if let (Some(id), Some(map)) = (&template_id, params.as_object_mut()) {
        map.insert("template_id".into(), Value::String(id.clone()));
    }
    let Some(id) = params.get("template_id").and_then(super::params::string) else {
        // Without an id the template is nil (`maybe_return_template_error`).
        return Ok(unprocessable("Template not found"));
    };
    let template = match super::templates::authorized_template(
        state,
        auth,
        headers,
        &id,
        loco_domain::ability::Action::Read,
    )
    .await?
    {
        Ok(template) => template,
        Err(response) => return Ok(response),
    };
    // `maybe_return_template_error`.
    if template.archived_at.is_some() {
        return Ok(unprocessable("Template has been archived"));
    }
    if blank(Some(&template.fields)) {
        return Ok(unprocessable("Template does not contain fields"));
    }
    if let Err(message) = validate::submission_create(&params) {
        return Ok(unprocessable(&message));
    }
    if let Some(map) = params.as_object_mut() {
        map.entry("send_email").or_insert(Value::Bool(true));
        map.entry("send_sms").or_insert(Value::Bool(false));
    }

    let is_send_email = !matches!(params.get("send_email"), Some(Value::Bool(false)))
        && params.get("send_email").and_then(Value::as_str) != Some("false");
    let emails = params
        .get("emails")
        .filter(|v| !v.is_null())
        .or_else(|| params.get("email"))
        .cloned();
    let from_emails = emails.as_ref().is_some_and(|e| !blank(Some(e)))
        && blank(params.get("submission"))
        && blank(params.get("submitters"));

    let (drafts, submitters_order) = if from_emails {
        let built = build_from_emails(
            state,
            auth,
            &template,
            &params,
            emails.as_ref().unwrap_or(&Value::Null),
            is_send_email,
        )
        .await?;
        (built, "random".to_owned())
    } else {
        let mut attrs = submissions_params(&params);
        let template_fields = array(Some(&template.fields)).to_vec();
        let template_submitters = array(Some(&template.submitters)).to_vec();
        for submission in &mut attrs {
            let Some(Value::Array(list)) = submission.get_mut("submitters") else {
                continue;
            };
            for (index, submitter) in list.iter_mut().enumerate() {
                let Some(map) = submitter.as_object_mut() else {
                    continue;
                };
                let index = submitter_index(map).unwrap_or(index);
                if let Err(Invalid(message)) = normalize_submitter_params(
                    map,
                    &template_fields,
                    &template_submitters,
                    index,
                    None,
                ) {
                    return Ok(unprocessable(&message));
                }
            }
        }
        let order = params
            .get("submitters_order")
            .filter(|v| !v.is_null())
            .or_else(|| params.get("order").filter(|v| !v.is_null()))
            .map_or_else(|| "preserved".to_owned(), ruby_to_s);
        let built = build_from_submitters(state, auth, &template, &params, attrs, &order).await?;
        (built, order)
    };
    let drafts = match drafts {
        Ok(drafts) => drafts,
        Err(Invalid(message)) => return Ok(unprocessable(&message)),
    };

    let saved = save(state, auth, &template, drafts, "api", &submitters_order).await?;

    let records: Vec<EventRecord> = saved
        .iter()
        .map(|(s, _)| EventRecord {
            id: s.id,
            account_id: s.account_id,
        })
        .collect();
    webhooks::enqueue(state, "submission.created", &records).await;
    send_submission_requests(state, &saved).await;
    let ids: Vec<i64> = saved.iter().map(|(s, _)| s.id).collect();
    crate::search::reindex_later(state, "Submission", &ids).await;

    render_created(state, auth, uri, &params, &saved).await
}

/// `build_create_json`.
async fn render_created(
    state: &AppState,
    auth: &Auth,
    uri: &Uri,
    params: &Value,
    saved: &[(Submission, Vec<Submitter>)],
) -> Result<Response, AppError> {
    let list: Vec<Submission> = saved.iter().map(|(s, _)| s.clone()).collect();
    let by_submission = saved.iter().map(|(s, r)| (s.id, r.clone())).collect();
    let data =
        SubmissionData::with_submitters(state, &list, by_submission, Load::default()).await?;
    let urls = Urls::new(state, auth.account.id).await?;
    let include_fields = params
        .get("include")
        .map(ruby_to_s)
        .is_some_and(|i| i.contains("fields"));
    let opts = SubmitterOpts {
        with_documents: false,
        with_urls: true,
        include_fields,
        ..SubmitterOpts::default()
    };
    let submitters: Vec<Value> = saved
        .iter()
        .flat_map(|(submission, submitters)| {
            submitters
                .iter()
                .map(|s| serialize::submitter_json(&urls, &data, submission, s, opts))
                .collect::<Vec<_>>()
        })
        .collect();
    if !uri.path().ends_with("/init") {
        return Ok(super::ok(&Value::Array(submitters)));
    }
    Ok(super::ok(&match saved {
        [(submission, _)] => json!({
            "id": submission.id,
            "submitters": submitters,
            "expire_at": serialize::opt_time(submission.expire_at),
            "created_at": serialize::time(submission.created_at),
        }),
        _ => json!({ "submitters": submitters }),
    }))
}

/// `POST /api/submissions`.
pub async fn create(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    uri: Uri,
    params: Params,
) -> Result<Response, AppError> {
    create_submissions(&state, &auth, &headers, &uri, None, params.0).await
}

/// `POST /api/submissions/init`.
pub async fn create_init(
    state: State<AppState>,
    auth: ApiAuth,
    headers: HeaderMap,
    uri: Uri,
    params: Params,
) -> Result<Response, AppError> {
    create(state, auth, headers, uri, params).await
}

/// `POST /api/submissions/emails`.
pub async fn create_emails(
    state: State<AppState>,
    auth: ApiAuth,
    headers: HeaderMap,
    uri: Uri,
    params: Params,
) -> Result<Response, AppError> {
    create(state, auth, headers, uri, params).await
}

/// `POST /api/templates/:template_id/submissions`.
pub async fn create_for_template(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    create_submissions(&state, &auth, &headers, &uri, Some(id), params.0).await
}
