//! Rails request parameters for the API: the query string merged with a JSON or
//! URL-encoded body (`request.parameters`), as one JSON value.
//!
//! Query and form values are strings, parsed with Rack's nesting rules (`a[b]=1`,
//! `a[]=1`, `a[][b]=1`); JSON bodies keep their types. Handlers read them with the
//! Rails-like helpers on [`Params`].

use axum::{
    Json,
    body::to_bytes,
    extract::{FromRequest, Request},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};

use crate::state::AppState;

/// Largest API request body (documents may be sent base64-encoded).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Params(pub Value);

impl Params {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// `params.key?(key)`.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.0.get(key).is_some()
    }

    /// `params[key].presence` as a string (numbers and booleans are stringified).
    #[must_use]
    pub fn str(&self, key: &str) -> Option<String> {
        string(self.0.get(key)?)
    }

    /// `params[key].in?(['true', true])`.
    #[must_use]
    pub fn is_true(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(is_true)
    }

    /// `params[key].to_i` for a present value.
    #[must_use]
    pub fn int(&self, key: &str) -> Option<i64> {
        self.str(key).map(|s| to_i(&s))
    }

    /// `params.key?(nested) ? params[nested] : params`, for controllers that accept both
    /// `{"submission": {...}}` and top-level attributes.
    #[must_use]
    pub fn nested_or_root(&self, key: &str) -> Value {
        match self.0.get(key) {
            Some(value @ Value::Object(_)) => value.clone(),
            _ => self.0.clone(),
        }
    }
}

/// `value.presence` as a string.
#[must_use]
pub fn string(value: &Value) -> Option<String> {
    let s = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!s.trim().is_empty()).then_some(s)
}

/// `value.in?(['true', true])`.
#[must_use]
pub fn is_true(value: &Value) -> bool {
    matches!(value, Value::Bool(true)) || value.as_str() == Some("true")
}

/// Ruby's `String#to_i`: leading whitespace, an optional sign and digits; else 0.
#[must_use]
pub fn to_i(value: &str) -> i64 {
    let s = value.trim_start();
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit() && c != '_')
        .unwrap_or(digits.len());
    digits[..end]
        .replace('_', "")
        .parse::<i64>()
        .map_or(0, |n| sign * n)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

impl FromRequest<AppState> for Params {
    type Rejection = Response;

    async fn from_request(request: Request, _state: &AppState) -> Result<Self, Response> {
        let mut params = request
            .uri()
            .query()
            .map_or_else(|| Value::Object(Map::new()), parse_nested_query);

        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_default();

        let bytes = to_bytes(request.into_body(), MAX_BODY_BYTES)
            .await
            .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE.into_response())?;

        let body = if bytes.is_empty() {
            None
        } else if content_type == "application/json" || content_type.ends_with("+json") {
            match serde_json::from_slice::<Value>(&bytes) {
                // `wrap_parameters false`: a non-object body is ignored, like Rails.
                Ok(value @ Value::Object(_)) => Some(value),
                Ok(_) => None,
                Err(err) => {
                    return Err(json_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        &format!("JSON parse error: {err}"),
                    ));
                }
            }
        } else if content_type == "application/x-www-form-urlencoded" {
            Some(parse_nested_query(&String::from_utf8_lossy(&bytes)))
        } else {
            None
        };

        if let (Value::Object(params), Some(Value::Object(body))) = (&mut params, body) {
            for (key, value) in body {
                params.insert(key, value);
            }
        }

        Ok(Self(params))
    }
}

/// `Rack::Utils.parse_nested_query`.
#[must_use]
pub fn parse_nested_query(query: &str) -> Value {
    let mut root = Value::Object(Map::new());
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key.is_empty() {
            continue;
        }
        normalize(&mut root, &key, Value::String(value.into_owned()));
    }
    root
}

/// `Rack::QueryParser#normalize_params`.
fn normalize(target: &mut Value, name: &str, value: Value) {
    let (head, rest) = match name.find('[') {
        Some(0) => {
            // A leading bracket: `[a]=1` is `a`.
            let close = name.find(']').unwrap_or(name.len());
            (&name[1..close], name.get(close + 1..).unwrap_or(""))
        }
        Some(i) => (&name[..i], &name[i..]),
        None => (name, ""),
    };
    let Value::Object(map) = target else {
        return;
    };

    if rest.is_empty() {
        map.insert(head.to_owned(), value);
        return;
    }

    if rest == "[]" {
        let entry = map
            .entry(head.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(items) = entry {
            items.push(value);
        }
        return;
    }

    if let Some(child) = rest.strip_prefix("[]") {
        // `a[][b]=1`: append a hash, or extend the last one if it lacks the key.
        let entry = map
            .entry(head.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(items) = entry else {
            return;
        };
        let reuse = items
            .last()
            .is_some_and(|last| last.is_object() && !nested_key_present(last, child));
        if !reuse {
            items.push(Value::Object(Map::new()));
        }
        if let Some(last) = items.last_mut() {
            normalize(last, child, value);
        }
        return;
    }

    if let Some(after) = rest.strip_prefix('[') {
        let close = after.find(']').unwrap_or(after.len());
        let child = format!(
            "{}{}",
            &after[..close],
            after.get(close + 1..).unwrap_or("")
        );
        let entry = map
            .entry(head.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        normalize(entry, &child, value);
    }
}

/// Whether the nested key path (`[b][c]`) already holds a value in `object`.
fn nested_key_present(object: &Value, path: &str) -> bool {
    let mut current = object;
    let mut rest = path;
    while let Some(after) = rest.strip_prefix('[') {
        let close = after.find(']').unwrap_or(after.len());
        let key = &after[..close];
        let Some(next) = current.get(key) else {
            return false;
        };
        current = next;
        rest = after.get(close + 1..).unwrap_or("");
        if rest.is_empty() || rest.starts_with("[]") {
            return true;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rack_nested_queries() {
        assert_eq!(
            parse_nested_query("a=1&b[c]=2&b[d][]=3&b[d][]=4"),
            json!({"a": "1", "b": {"c": "2", "d": ["3", "4"]}})
        );
        assert_eq!(
            parse_nested_query(
                "submitters[][email]=a%40x.test&submitters[][role]=A&submitters[][email]=b%40x.test"
            ),
            json!({"submitters": [{"email": "a@x.test", "role": "A"}, {"email": "b@x.test"}]})
        );
        assert_eq!(parse_nested_query("q=a+b"), json!({"q": "a b"}));
    }

    #[test]
    fn converts_like_ruby() {
        assert_eq!(to_i("42"), 42);
        assert_eq!(to_i(" 7abc"), 7);
        assert_eq!(to_i("abc"), 0);
        assert_eq!(to_i("-3"), -3);
        assert!(is_true(&json!(true)));
        assert!(is_true(&json!("true")));
        assert!(!is_true(&json!("1")));
    }
}
