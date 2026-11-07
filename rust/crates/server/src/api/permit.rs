//! Strong parameters (`ActionController::Parameters#permit`): keep only declared keys,
//! with scalar values where scalars are declared.

use serde_json::{Map, Value};

/// How one permitted key may look.
#[derive(Clone, Copy, Debug)]
pub enum Rule {
    /// `:key`: a scalar (string, number, boolean or null).
    Scalar,
    /// `:key` and `key: []` together: a scalar or an array of scalars.
    ScalarOrScalars,
    /// `key: []`: an array of scalars.
    Scalars,
    /// `key: {}`: any hash.
    AnyHash,
    /// `key: [...]` / `key: %i[...]`: a hash with these keys.
    Object(&'static [(&'static str, Rule)]),
    /// `key: [[...]]`: an array of hashes with these keys.
    Objects(&'static [(&'static str, Rule)]),
    /// Either an array of hashes or one hash (`{ key => [rules] }, { key => rules }`).
    ObjectOrObjects(&'static [(&'static str, Rule)]),
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

/// Rails accepts `{"0": {...}, "1": {...}}` where an array of hashes is permitted.
fn as_object_list(value: &Value) -> Option<Vec<&Value>> {
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Object(map)
            if !map.is_empty() && map.keys().all(|k| k.chars().all(|c| c.is_ascii_digit())) =>
        {
            Some(map.values().collect())
        }
        _ => None,
    }
}

fn apply(value: &Value, rule: Rule) -> Option<Value> {
    match rule {
        Rule::Scalar => is_scalar(value).then(|| value.clone()),
        Rule::Scalars => match value {
            Value::Array(items) if items.iter().all(is_scalar) => Some(value.clone()),
            _ => None,
        },
        Rule::ScalarOrScalars => apply(value, Rule::Scalar).or_else(|| apply(value, Rule::Scalars)),
        Rule::AnyHash => value.is_object().then(|| value.clone()),
        Rule::Object(rules) => value
            .as_object()
            .map(|map| Value::Object(permit_map(map, rules))),
        Rule::Objects(rules) => as_object_list(value).map(|items| {
            Value::Array(
                items
                    .into_iter()
                    .filter_map(Value::as_object)
                    .map(|map| Value::Object(permit_map(map, rules)))
                    .collect(),
            )
        }),
        Rule::ObjectOrObjects(rules) => {
            apply(value, Rule::Objects(rules)).or_else(|| apply(value, Rule::Object(rules)))
        }
    }
}

/// `params.permit(rules)` on a hash.
#[must_use]
pub fn permit_map(map: &Map<String, Value>, rules: &[(&str, Rule)]) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, rule) in rules {
        if let Some(value) = map.get(*key)
            && let Some(value) = apply(value, *rule)
        {
            out.insert((*key).to_owned(), value);
        }
    }
    out
}

/// `params.permit(rules)`; a non-hash gives an empty hash.
#[must_use]
pub fn permit(value: &Value, rules: &[(&str, Rule)]) -> Map<String, Value> {
    value
        .as_object()
        .map(|map| permit_map(map, rules))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const RULES: &[(&str, Rule)] = &[
        ("name", Rule::Scalar),
        ("tags", Rule::Scalars),
        ("prefs", Rule::AnyHash),
        ("items", Rule::Objects(&[("id", Rule::Scalar)])),
    ];

    #[test]
    fn permits_like_strong_parameters() {
        let value = json!({
            "name": "a", "admin": true, "tags": ["x", 1], "prefs": {"a": {"b": 1}},
            "items": [{"id": 1, "secret": 2}, "junk"],
        });
        assert_eq!(
            Value::Object(permit(&value, RULES)),
            json!({"name": "a", "tags": ["x", 1], "prefs": {"a": {"b": 1}}, "items": [{"id": 1}]})
        );
        assert_eq!(
            Value::Object(permit(
                &json!({"name": ["no"], "items": {"0": {"id": 3}}}),
                RULES
            )),
            json!({"items": [{"id": 3}]})
        );
    }
}
