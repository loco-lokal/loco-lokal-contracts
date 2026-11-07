//! Pure parts of `Submitters::SubmitValues`, `Submissions.filtered_conditions_*` and
//! `Submitters.current_submitter_order?`: field conditions, default values, value
//! normalization and validation over the JSON documents stored in `submissions`.

#![allow(clippy::implicit_hasher, reason = "callers use the std hasher")]

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

/// `SubmitValues::NONEDITABLE_FIELD_TYPES`.
pub const NONEDITABLE_FIELD_TYPES: &[&str] = &["stamp", "heading", "strikethrough"];
/// `SubmitValues::REQUIRED_FIELD_TYPES`: required even when `validate_required` is off.
pub const REQUIRED_FIELD_TYPES: &[&str] = &["payment", "kba", "verification"];
/// `SubmitValues::CHOICE_FIELD_TYPES`.
pub const CHOICE_FIELD_TYPES: &[&str] = &["radio", "multiple", "select"];

/// `value.blank?`.
#[must_use]
pub fn blank(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        Some(_) => false,
    }
}

#[must_use]
pub fn present(value: Option<&Value>) -> bool {
    !blank(value)
}

/// `Array.wrap(value)`.
#[must_use]
pub fn wrap(value: Option<&Value>) -> Vec<Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `submission.fields_uuid_index`.
#[must_use]
pub fn fields_uuid_index(fields: &[Value]) -> HashMap<String, &Value> {
    fields
        .iter()
        .filter_map(|f| Some((str_of(f, "uuid")?.to_owned(), f)))
        .collect()
}

/// `String#to_f` for condition values (strings or numbers).
fn to_f(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => leading_float(s),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => 0.0,
    }
}

fn leading_float(s: &str) -> f64 {
    let s = s.trim_start();
    let mut end = 0;
    let bytes = s.as_bytes();
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            b'_' if seen_digit => {}
            _ => break,
        }
        end += 1;
    }
    s[..end].replace('_', "").parse().unwrap_or(0.0)
}

/// The value a choice option stands for: its `value`, or `"<Option> N"`.
#[must_use]
pub fn option_name(field: &Value, option: &Value, option_word: &str) -> String {
    if let Some(v) = option.get("value").filter(|v| present(Some(v))) {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    let index = field
        .get("options")
        .and_then(Value::as_array)
        .and_then(|options| options.iter().position(|o| o == option))
        .unwrap_or(0);
    format!("{option_word} {}", index + 1)
}

/// `SubmitValues.check_field_condition`. `option_word` is `I18n.t('option')`.
#[must_use]
pub fn check_field_condition(
    condition: &Value,
    values: &Map<String, Value>,
    index: &HashMap<String, &Value>,
    option_word: &str,
) -> bool {
    let field_uuid = str_of(condition, "field_uuid").unwrap_or_default();
    let value = values.get(field_uuid);
    let field = index.get(field_uuid).copied();
    let expected = condition.get("value");
    let is_number = field.and_then(|f| str_of(f, "type")) == Some("number");

    let find_option = |field: &Value| -> Option<String> {
        let options = field.get("options")?.as_array()?;
        let option = options.iter().find(|o| o.get("uuid") == expected)?;
        Some(option_name(field, option, option_word))
    };
    let includes = |name: &str| wrap(value).iter().any(|v| v.as_str() == Some(name));

    match str_of(condition, "action").unwrap_or_default() {
        "empty" | "unchecked" => blank(value),
        "not_empty" | "checked" => present(value),
        "equal" if is_number => {
            if blank(value) || blank(expected) {
                return false;
            }
            (to_f(value.unwrap()) - to_f(expected.unwrap())).abs() < f64::EPSILON
        }
        "not_equal" if is_number => {
            if blank(value) || blank(expected) {
                return false;
            }
            (to_f(value.unwrap()) - to_f(expected.unwrap())).abs() > f64::EPSILON
        }
        "greater_than" => {
            if field.is_none() || blank(value) || blank(expected) {
                return false;
            }
            to_f(value.unwrap()) > to_f(expected.unwrap())
        }
        "less_than" => {
            if field.is_none() || blank(value) || blank(expected) {
                return false;
            }
            to_f(value.unwrap()) < to_f(expected.unwrap())
        }
        "equal" | "contains" => {
            let Some(field) = field else { return true };
            if field.get("options").is_none_or(Value::is_null) {
                return wrap(value).iter().any(|v| Some(v) == expected);
            }
            find_option(field).is_some_and(|name| includes(&name))
        }
        "not_equal" | "does_not_contain" => {
            let Some(field) = field else { return true };
            if field.get("options").is_none_or(Value::is_null) {
                return false;
            }
            find_option(field).is_some_and(|name| !includes(&name))
        }
        _ => true,
    }
}

/// Folds condition results: `or` merges with the previous result; all must hold.
fn fold_conditions(conditions: &[Value], mut check: impl FnMut(&Value) -> bool) -> bool {
    let mut acc: Vec<bool> = Vec::new();
    for condition in conditions {
        let result = check(condition);
        if str_of(condition, "operation") == Some("or") {
            let previous = acc.pop().unwrap_or(false);
            acc.push(previous || result);
        } else {
            acc.push(result);
        }
    }
    !acc.contains(&false)
}

fn conditions(item: &Value) -> &[Value] {
    item.get("conditions")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// `SubmitValues.check_field_conditions`.
#[must_use]
pub fn check_field_conditions(
    values: &Map<String, Value>,
    field: &Value,
    index: &HashMap<String, &Value>,
    option_word: &str,
) -> bool {
    fold_conditions(conditions(field), |c| {
        check_field_condition(c, values, index, option_word)
    })
}

/// `Submissions.check_item_conditions`: conditions on `include_submitter_uuid`'s own
/// fields pass (and are collected into `submitter_conditions`).
#[must_use]
pub fn check_item_conditions(
    item: &Value,
    values: &Map<String, Value>,
    index: &HashMap<String, &Value>,
    include_submitter_uuid: Option<&str>,
    mut submitter_conditions: Option<&mut Vec<Value>>,
    option_word: &str,
) -> bool {
    fold_conditions(conditions(item), |condition| {
        let owner = str_of(condition, "field_uuid")
            .and_then(|uuid| index.get(uuid))
            .and_then(|f| str_of(f, "submitter_uuid"));
        if owner == include_submitter_uuid {
            if let Some(acc) = submitter_conditions.as_deref_mut() {
                acc.push(condition.clone());
            }
            true
        } else {
            check_field_condition(condition, values, index, option_word)
        }
    })
}

/// `Submissions.filtered_conditions_schema`.
#[must_use]
pub fn filtered_conditions_schema(
    schema: &[Value],
    fields: &[Value],
    values: &Map<String, Value>,
    include_submitter_uuid: Option<&str>,
    option_word: &str,
) -> Vec<Value> {
    let index = fields_uuid_index(fields);
    schema
        .iter()
        .filter(|item| {
            conditions(item).is_empty()
                || check_item_conditions(
                    item,
                    values,
                    &index,
                    include_submitter_uuid,
                    None,
                    option_word,
                )
        })
        .cloned()
        .collect()
}

/// `Submissions.filtered_conditions_fields(submitter)`: the submitter's fields whose
/// conditions on other parties hold, keeping only the conditions on its own fields.
#[must_use]
pub fn filtered_conditions_fields(
    fields: &[Value],
    values: &Map<String, Value>,
    submitter_uuid: &str,
    option_word: &str,
) -> Vec<Value> {
    let index = fields_uuid_index(fields);
    fields
        .iter()
        .filter(|f| str_of(f, "submitter_uuid") == Some(submitter_uuid))
        .filter_map(|field| {
            if conditions(field).is_empty() {
                return Some(field.clone());
            }
            let mut own = Vec::new();
            if !check_item_conditions(
                field,
                values,
                &index,
                Some(submitter_uuid),
                Some(&mut own),
                option_word,
            ) {
                return None;
            }
            let mut field = field.clone();
            if own.as_slice() != conditions(&field) {
                field["conditions"] = Value::Array(own);
            }
            Some(field)
        })
        .collect()
}

/// `Templates.build_field_areas_index`: `attachment_uuid → page → [(area, field)]`.
#[must_use]
pub fn field_areas_index(fields: &[Value]) -> HashMap<String, HashMap<i64, Vec<(Value, Value)>>> {
    let mut index: HashMap<String, HashMap<i64, Vec<(Value, Value)>>> = HashMap::new();
    for field in fields {
        for area in field
            .get("areas")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let attachment = str_of(area, "attachment_uuid")
                .unwrap_or_default()
                .to_owned();
            let page = area.get("page").and_then(Value::as_i64).unwrap_or_default();
            index
                .entry(attachment)
                .or_default()
                .entry(page)
                .or_default()
                .push((area.clone(), field.clone()));
        }
    }
    index
}

/// A submitter as the ordering rules see it.
#[derive(Clone, Copy, Debug)]
pub struct Party<'a> {
    pub uuid: &'a str,
    pub completed: bool,
    pub viewer: bool,
}

/// `Submitters.current_submitter_order?`: every party before this one (by `order` group,
/// or by position) has completed, is a viewer, or does not exist yet.
#[must_use]
pub fn current_submitter_order(
    submitter_uuid: &str,
    submitter_is_viewer: bool,
    template_submitters: &[Value],
    parties: &[Party<'_>],
) -> bool {
    let find = |uuid: &str| parties.iter().find(|p| p.uuid == uuid);
    let item_uuid = |item: &Value| str_of(item, "uuid").unwrap_or_default().to_owned();
    let position = template_submitters
        .iter()
        .position(|e| str_of(e, "uuid") == Some(submitter_uuid));

    let before: Vec<String> = if template_submitters
        .iter()
        .any(|s| s.get("order").is_some_and(|o| !o.is_null()))
    {
        let mut groups: Vec<(i64, Vec<&Value>)> = Vec::new();
        for (index, item) in template_submitters.iter().enumerate() {
            let key = item
                .get("order")
                .and_then(Value::as_i64)
                .unwrap_or(i64::try_from(index).unwrap_or_default());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, group)) => group.push(item),
                None => groups.push((key, vec![item])),
            }
        }
        groups.sort_by_key(|(k, _)| *k);
        let current = groups.iter().position(|(_, group)| {
            group
                .iter()
                .any(|s| str_of(s, "uuid") == Some(submitter_uuid))
        });
        groups
            .iter()
            .take(current.unwrap_or(0))
            .flat_map(|(_, group)| group.iter().map(|i| item_uuid(i)))
            .collect()
    } else if submitter_is_viewer {
        let current = position.unwrap_or(0);
        let preceding = template_submitters[..current]
            .iter()
            .rposition(|e| !find(str_of(e, "uuid").unwrap_or_default()).is_some_and(|p| p.viewer));
        template_submitters
            .iter()
            .take(preceding.unwrap_or(0))
            .map(item_uuid)
            .collect()
    } else {
        template_submitters
            .iter()
            .take(position.unwrap_or(0))
            .map(item_uuid)
            .collect()
    };

    before
        .iter()
        .all(|uuid| find(uuid).is_none_or(|p| p.viewer || p.completed))
}

/// How a form step's values arrive (`cast_boolean`, `cast_number`, `normalize_phone`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Casts {
    pub boolean: bool,
    pub number: bool,
    pub phone: bool,
}

/// `SubmitValues.normalized_values` over form parameters (strings, or arrays of them).
#[must_use]
pub fn normalized_values(values: &Map<String, Value>, casts: Casts) -> Map<String, Value> {
    values
        .iter()
        .map(|(k, v)| {
            let v = if casts.boolean {
                Value::Bool(v.as_str() == Some("true"))
            } else if casts.number {
                if v.as_str() == Some("") {
                    Value::Null
                } else {
                    let f = to_f(v);
                    if f.fract() == 0.0 && f.abs() < 9.0e15 {
                        #[allow(clippy::cast_possible_truncation, reason = "checked range")]
                        Value::from(f as i64)
                    } else {
                        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
                    }
                }
            } else if casts.phone {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                Value::String(
                    s.chars()
                        .filter(|c| c.is_ascii_digit() || *c == '+')
                        .collect(),
                )
            } else if let Value::Array(items) = v {
                Value::Array(items.iter().filter(|i| present(Some(i))).cloned().collect())
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect()
}

/// Why a submitted value was refused (`SubmitValues::ValidationError` messages).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

fn invalid(message: &str) -> ValidationError {
    ValidationError(message.to_owned())
}

/// `SubmitValues.validate_value!`.
pub fn validate_value(
    value: &Value,
    field: Option<&Value>,
    submitter_uuid: &str,
    option_word: &str,
) -> Result<(), ValidationError> {
    let field = field.ok_or_else(|| invalid("Missing field"))?;
    if str_of(field, "submitter_uuid") != Some(submitter_uuid) {
        return Err(invalid("Invalid field"));
    }
    if field.get("readonly") == Some(&Value::Bool(true)) {
        return Err(invalid("Read-only field"));
    }
    let field_type = str_of(field, "type").unwrap_or_default();
    if field_type == "checkbox" && !value.is_boolean() {
        return Err(invalid("Value is invalid"));
    }
    if CHOICE_FIELD_TYPES.contains(&field_type) && present(Some(value)) {
        let shape_ok = if field_type == "multiple" {
            value.is_array()
        } else {
            value.is_string()
        };
        if !shape_ok {
            return Err(invalid("Value is invalid"));
        }
        let names: Vec<String> = field
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|o| option_name(field, o, option_word))
            .collect();
        if !wrap(Some(value))
            .iter()
            .all(|item| item.as_str().is_some_and(|s| names.iter().any(|n| n == s)))
        {
            return Err(invalid("Value is invalid"));
        }
    }
    Ok(())
}

/// `SubmitValues.required_editable_field?`.
#[must_use]
pub fn required_editable_field(field: &Value) -> bool {
    let field_type = str_of(field, "type").unwrap_or_default();
    if NONEDITABLE_FIELD_TYPES.contains(&field_type) {
        return false;
    }
    present(field.get("required")) && blank(field.get("readonly"))
}

/// The values `maybe_remove_condition_values` drops for one submitter: fields on
/// documents hidden by conditions and fields whose own conditions fail. Returns the
/// remaining values and the required editable fields that are still shown.
#[must_use]
pub fn remove_condition_values(
    fields: &[Value],
    submitter_uuid: &str,
    own_values: &Map<String, Value>,
    other_values: &Map<String, Value>,
    visible_attachments: Option<&HashSet<String>>,
    has_other_submitters: bool,
    option_word: &str,
) -> (Map<String, Value>, Vec<String>) {
    let index = fields_uuid_index(fields);
    let mut values = own_values.clone();
    // `merge_submitters_values`: the other parties' values, overridden by this one's.
    let merged = |values: &Map<String, Value>| {
        let mut all = other_values.clone();
        all.extend(values.clone());
        all
    };
    let mut required: Vec<String> = Vec::new();
    // Once another party's values are needed they are used for the remaining fields.
    let mut use_merged = visible_attachments.is_some();

    for field in fields {
        if str_of(field, "submitter_uuid") != Some(submitter_uuid) {
            continue;
        }
        let uuid = str_of(field, "uuid").unwrap_or_default().to_owned();
        if required_editable_field(field) && !required.contains(&uuid) {
            required.push(uuid.clone());
        }
        let mut drop = false;
        if let Some(visible) = visible_attachments {
            let areas = field.get("areas").and_then(Value::as_array);
            if let Some(areas) = areas.filter(|a| !a.is_empty())
                && !areas.iter().any(|a| {
                    str_of(a, "attachment_uuid").is_some_and(|uuid| visible.contains(uuid))
                })
            {
                drop = true;
            }
        }
        if !drop && !conditions(field).is_empty() {
            let other_submitter = conditions(field).iter().any(|c| {
                str_of(c, "field_uuid")
                    .and_then(|u| index.get(u))
                    .and_then(|f| str_of(f, "submitter_uuid"))
                    != Some(submitter_uuid)
            });
            use_merged |= has_other_submitters && other_submitter;
            let pass = if use_merged {
                check_field_conditions(&merged(&values), field, &index, option_word)
            } else {
                check_field_conditions(&values, field, &index, option_word)
            };
            drop = !pass;
        }
        if drop {
            values.remove(&uuid);
            required.retain(|u| *u != uuid);
        }
    }
    (values, required)
}

/// Formats a `{{date}}`-style variable for the current time, by variable name.
pub type Clock<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// `SubmitValues::VARIABLE_REGEXP` replacement in `replace_default_variables`, for the
/// keys that need no clock (`id`, `role`, `email`, `phone`, `name`). `clock` handles
/// `time`, `hour`, `minute`, `day`, `month`, `year` and `date` when given.
#[must_use]
pub fn replace_default_variables(
    value: &Value,
    attrs: &Map<String, Value>,
    clock: Option<&Clock<'_>>,
) -> Option<Value> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\{\{?(\w+)\}\}?").expect("valid"));
    match value {
        Value::Bool(_) | Value::Number(_) | Value::Array(_) => return Some(value.clone()),
        _ if blank(Some(value)) => return None,
        _ => {}
    }
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let replaced = RE.replace_all(&text, |caps: &regex::Captures<'_>| {
        let whole = caps[0].to_owned();
        let key = &caps[1];
        match key {
            "id" => attrs
                .get("submission_id")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
            "time" | "hour" | "minute" | "day" | "month" | "year" | "date" => {
                clock.and_then(|c| c(key)).unwrap_or(whole)
            }
            "role" | "email" | "phone" | "name" => match attrs.get(key) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => whole,
                Some(other) => other.to_string(),
            },
            _ => whole,
        }
    });
    Some(Value::String(replaced.into_owned()))
}

/// `TimeUtils.format_with_time?`.
#[must_use]
pub fn format_with_time(format: &str) -> bool {
    format.chars().any(|c| "HhAasz".contains(c))
}

/// `TimeUtils.month_only_format?`.
#[must_use]
pub fn month_only_format(format: &str) -> bool {
    !format.trim().is_empty() && !format.chars().any(|c| "DdHhAasz".contains(c))
}

/// `TimeUtils.current_date_value(format, timezone)` for `now` in the zone.
#[must_use]
pub fn current_date_value(
    format: Option<&str>,
    now_utc: chrono::DateTime<chrono::Utc>,
    tz: chrono_tz::Tz,
) -> String {
    let format = format.unwrap_or_default();
    if format_with_time(format) {
        now_utc.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else if month_only_format(format) {
        now_utc.with_timezone(&tz).format("%Y-%m").to_string()
    } else {
        now_utc.with_timezone(&tz).format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test helper taking json! literals"
    )]
    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn checks_conditions_like_rails() {
        let fields = vec![
            json!({"uuid": "a", "type": "text", "submitter_uuid": "s1"}),
            json!({"uuid": "b", "type": "radio", "submitter_uuid": "s1",
                   "options": [{"uuid": "o1", "value": "Yes"}, {"uuid": "o2", "value": ""}]}),
            json!({"uuid": "n", "type": "number", "submitter_uuid": "s1"}),
        ];
        let index = fields_uuid_index(&fields);
        let values = map(json!({"a": "x", "b": "Option 2", "n": "3.0"}));
        let check = |c: Value| check_field_condition(&c, &values, &index, "Option");
        assert!(check(json!({"field_uuid": "a", "action": "not_empty"})));
        assert!(!check(json!({"field_uuid": "a", "action": "empty"})));
        assert!(check(
            json!({"field_uuid": "b", "action": "equal", "value": "o2"})
        ));
        assert!(check(
            json!({"field_uuid": "b", "action": "not_equal", "value": "o1"})
        ));
        assert!(check(
            json!({"field_uuid": "n", "action": "equal", "value": "3"})
        ));
        assert!(check(
            json!({"field_uuid": "n", "action": "greater_than", "value": "2"})
        ));
        assert!(check(
            json!({"field_uuid": "zz", "action": "equal", "value": "1"})
        ));

        let field = json!({"conditions": [
            {"field_uuid": "a", "action": "empty"},
            {"field_uuid": "a", "action": "not_empty", "operation": "or"}
        ]});
        assert!(check_field_conditions(&values, &field, &index, "Option"));
    }

    #[test]
    fn orders_submitters() {
        let items = vec![
            json!({"uuid": "a"}),
            json!({"uuid": "b"}),
            json!({"uuid": "c"}),
        ];
        let parties = [
            Party {
                uuid: "a",
                completed: true,
                viewer: false,
            },
            Party {
                uuid: "b",
                completed: false,
                viewer: false,
            },
            Party {
                uuid: "c",
                completed: false,
                viewer: false,
            },
        ];
        assert!(current_submitter_order("b", false, &items, &parties));
        assert!(!current_submitter_order("c", false, &items, &parties));
        let grouped = vec![
            json!({"uuid": "a", "order": 0}),
            json!({"uuid": "b", "order": 1}),
            json!({"uuid": "c", "order": 1}),
        ];
        assert!(current_submitter_order("c", false, &grouped, &parties));
    }

    #[test]
    fn normalizes_values() {
        let values = map(json!({"a": "2", "b": "2.5", "c": ""}));
        let out = normalized_values(
            &values,
            Casts {
                number: true,
                ..Casts::default()
            },
        );
        assert_eq!(Value::Object(out), json!({"a": 2, "b": 2.5, "c": null}));
        let out = normalized_values(&map(json!({"a": ["x", ""]})), Casts::default());
        assert_eq!(Value::Object(out), json!({"a": ["x"]}));
    }

    #[test]
    fn validates_choices() {
        let field = json!({"uuid": "b", "type": "select", "submitter_uuid": "s",
                           "options": [{"uuid": "o1", "value": "Yes"}]});
        assert!(validate_value(&json!("Yes"), Some(&field), "s", "Option").is_ok());
        assert_eq!(
            validate_value(&json!("No"), Some(&field), "s", "Option")
                .unwrap_err()
                .0,
            "Value is invalid"
        );
        assert_eq!(
            validate_value(&json!("Yes"), Some(&field), "t", "Option")
                .unwrap_err()
                .0,
            "Invalid field"
        );
    }

    #[test]
    fn replaces_default_variables() {
        let attrs = map(json!({"name": "Ann", "submission_id": 4, "email": null}));
        let out =
            replace_default_variables(&json!("{{name}} #{{id}} {{email}} {date}"), &attrs, None);
        assert_eq!(out, Some(json!("Ann #4 {{email}} {date}")));
    }
}
