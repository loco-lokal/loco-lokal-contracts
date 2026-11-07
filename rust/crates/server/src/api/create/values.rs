//! `Submitters::NormalizeValues`: prefilled values keyed by field name or uuid become
//! values keyed by field uuid, cast to the field type.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::{Map, Value};

/// Field types whose values are files (`initials signature image file stamp`).
const ATTACHMENT_TYPES: &[&str] = &["initials", "signature", "image", "file", "stamp"];
const NON_PREFILLABLE_FIELD_TYPES: &[&str] = &["payment", "kba", "verification"];

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

/// `String#inspect` for the plain strings of error messages.
fn inspect_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `Object#to_s` (`Array#to_s` and `Hash#to_s` are `inspect`).
#[must_use]
/// `Float#to_i`: truncates toward zero, saturating at the `i64` range.
#[allow(clippy::cast_possible_truncation)] // truncation is the point
fn float_to_i(f: f64) -> i64 {
    f as i64
}

pub fn ruby_to_s(value: &Value) -> String {
    fn inspect(value: &Value) -> String {
        match value {
            Value::String(s) => inspect_str(s),
            Value::Null => "nil".into(),
            other => ruby_to_s(other),
        }
    }
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| !n.is_i64() && !n.is_u64()) {
                ruby_float(f)
            } else {
                n.to_string()
            }
        }
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(inspect).collect::<Vec<_>>().join(", ")
            )
        }
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{} => {}", inspect_str(k), inspect(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `Float#to_s` for ordinary magnitudes (`2.0`, `1.5`).
fn ruby_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

/// `String#to_f`: the leading decimal number, or 0.
fn str_to_f(s: &str) -> f64 {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_digit = false;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
        seen_digit = true;
    }
    if end < bytes.len() && bytes[end] == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
    {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if seen_digit && end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
        let mut exp = end + 1;
        if matches!(bytes.get(exp), Some(b'+' | b'-')) {
            exp += 1;
        }
        if bytes.get(exp).is_some_and(u8::is_ascii_digit) {
            end = exp;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
    }
    s[..end].parse().unwrap_or(0.0)
}

/// `String#to_i`: the leading integer, or 0.
fn str_to_i(s: &str) -> i64 {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && matches!(c, '+' | '-')))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    s[..end].parse().unwrap_or(0)
}

const TRUE_VALUES: &[&str] = &["1", "true", "TRUE", "True", "yes", "YES", "Yes"];
const FALSE_VALUES: &[&str] = &["0", "false", "FALSE", "False", "no", "NO", "No"];

/// `NormalizeValues.normalize_value`.
#[must_use]
pub fn normalize_value(field: &Value, value: &Value) -> Value {
    let field_type = field
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if field_type == "checkbox" {
        match value {
            Value::Bool(b) => return Value::Bool(*b),
            Value::String(s) if TRUE_VALUES.contains(&s.as_str()) => return Value::Bool(true),
            Value::String(s) if FALSE_VALUES.contains(&s.as_str()) => return Value::Bool(false),
            _ => {}
        }
    }
    if blank(Some(value)) {
        return Value::Null;
    }
    match field_type {
        "text" => Value::String(ruby_to_s(value)),
        "number" => {
            let (f, i) = match value {
                Value::Number(n) => (
                    n.as_f64().unwrap_or(0.0),
                    n.as_i64()
                        .unwrap_or_else(|| float_to_i(n.as_f64().unwrap_or(0.0))),
                ),
                Value::String(s) => (str_to_f(s), str_to_i(s)),
                _ => return value.clone(),
            };
            if f % 1.0 == 0.0 {
                Value::from(i)
            } else {
                Value::from(f)
            }
        }
        "date" if value.as_str() != Some("{{date}}") => normalize_date(field, value),
        _ => value.clone(),
    }
}

/// `TimeUtils.format_with_time?`.
fn format_with_time(format: &str) -> bool {
    format.contains(['H', 'h', 'A', 'a', 's', 'z'])
}

/// `TimeUtils.month_only_format?`.
fn month_only_format(format: &str) -> bool {
    !format.trim().is_empty() && !format.contains(['D', 'd', 'H', 'h', 'A', 'a', 's', 'z'])
}

/// `TimeUtils::TOKEN_REGEX` replaced with `ALL_FORMATS`, in chrono's parsing syntax.
fn strptime_pattern(format: &str) -> String {
    const TOKENS: &[(&str, &str)] = &[
        ("MMMM", "%B"),
        ("MMM", "%b"),
        ("MM", "%m"),
        ("M", "%m"),
        ("DD", "%d"),
        ("D", "%d"),
        ("YYYY", "%Y"),
        ("YYY", "%Y"),
        ("YY", "%y"),
        ("HH", "%H"),
        ("hh", "%I"),
        ("H", "%H"),
        ("h", "%I"),
        ("mm", "%M"),
        ("m", "%M"),
        ("ss", "%S"),
        ("s", "%S"),
        ("A", "%p"),
        ("a", "%p"),
        ("z", "%Z"),
    ];
    let mut out = String::new();
    let mut rest = format;
    'outer: while !rest.is_empty() {
        for (token, to) in TOKENS {
            if let Some(r) = rest.strip_prefix(token) {
                out.push_str(to);
                rest = r;
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        if c == '%' {
            out.push_str("%%");
        } else {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn epoch(value: i64) -> Option<DateTime<Utc>> {
    let digits: String = value.to_string().chars().take(10).collect();
    DateTime::from_timestamp(digits.parse().ok()?, 0)
}

/// `TimeUtils.parse_date_string` (in UTC, the app's zone).
fn parse_date_string(value: &str, format: &str) -> Option<NaiveDateTime> {
    let with_time = format_with_time(format);
    let format = if with_time {
        format.to_owned()
    } else {
        format.to_uppercase()
    };
    let pattern = strptime_pattern(&format);
    if with_time {
        NaiveDateTime::parse_from_str(value, &pattern).ok()
    } else if pattern.contains("%d") {
        NaiveDate::parse_from_str(value, &pattern)
            .ok()
            .map(|d| d.and_time(chrono::NaiveTime::MIN))
    } else {
        // Month-only patterns: Date.strptime defaults the day to 1.
        NaiveDate::parse_from_str(&format!("1 {value}"), &format!("%d {pattern}"))
            .ok()
            .map(|d| d.and_time(chrono::NaiveTime::MIN))
    }
}

fn iso8601(t: DateTime<Utc>) -> Value {
    Value::String(t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// `NormalizeValues.normalize_date`; an unparseable value is kept as given.
fn normalize_date(field: &Value, value: &Value) -> Value {
    let format = field
        .pointer("/preferences/format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let int = value.as_i64();
    let text = value.as_str();
    let parsed = if format_with_time(format) {
        match (int, text) {
            (Some(i), _) => epoch(i).map(iso8601),
            (_, Some(s)) if has_iso_time(s) => parse_iso_time(s).map(iso8601),
            (_, Some(s)) => parse_date_string(s, format).map(|t| iso8601(t.and_utc())),
            _ => None,
        }
    } else if month_only_format(format) {
        match (int, text) {
            (Some(i), _) => epoch(i).map(|t| Value::String(t.format("%Y-%m").to_string())),
            (_, Some(s)) if is_month_value(s) => Some(Value::String(s.to_owned())),
            (_, Some(s)) => {
                parse_date_string(s, format).map(|t| Value::String(t.format("%Y-%m").to_string()))
            }
            _ => None,
        }
    } else if let Some(i) = int {
        epoch(i).map(|t| Value::String(t.date_naive().to_string()))
    } else if let Some(s) = text {
        let shape = |v: &str| -> String {
            v.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        '0'
                    } else {
                        c
                    }
                })
                .collect()
        };
        if shape(s) == shape(format) {
            parse_date_string(s, format).map(|t| Value::String(t.date().to_string()))
        } else {
            ruby_date_parse(s).map(|d| Value::String(d.to_string()))
        }
    } else {
        None
    };
    parsed.unwrap_or_else(|| value.clone())
}

fn has_iso_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(6).any(|w| {
        w[0] == b'T'
            && w[1].is_ascii_digit()
            && w[2].is_ascii_digit()
            && w[3] == b':'
            && w[4].is_ascii_digit()
            && w[5].is_ascii_digit()
    })
}

fn is_month_value(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit())
}

/// `Time.iso8601`: an offset, or UTC without one.
fn parse_iso_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            [
                "%Y-%m-%dT%H:%M:%S%.f",
                "%Y-%m-%dT%H:%M:%S",
                "%Y-%m-%dT%H:%M",
            ]
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
            .map(|t| t.and_utc())
        })
}

const MONTHS: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month_number(word: &str) -> Option<u32> {
    let word = word.to_lowercase();
    if word.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| word.starts_with(m))
        .and_then(|i| u32::try_from(i + 1).ok())
}

/// Two-digit years as `Date.parse` completes them.
fn complete_year(digits: &str, year: i32) -> i32 {
    if digits.len() > 2 {
        year
    } else if year >= 69 {
        1900 + year
    } else {
        2000 + year
    }
}

/// The common cases of `Date.parse`: ISO dates (also with a time), `YYYYMMDD`,
/// `DD.MM.YYYY`, `DD/MM/YYYY`, `YY/MM/DD`, and dates with English month names.
#[must_use]
pub fn ruby_date_parse(input: &str) -> Option<NaiveDate> {
    let s = input.trim();
    let date = |y: i32, m: u32, d: u32| NaiveDate::from_ymd_opt(y, m, d);

    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        return date(
            s[..4].parse().ok()?,
            s[4..6].parse().ok()?,
            s[6..].parse().ok()?,
        );
    }
    let head = s.split(['T', ' ']).next().unwrap_or(s);
    for sep in ['-', '/', '.'] {
        let parts: Vec<&str> = head.split(sep).collect();
        if parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            let n = |i: usize| parts[i].parse::<i32>().ok();
            return if parts[0].len() > 2 || sep == '-' {
                date(
                    complete_year(parts[0], n(0)?),
                    u32::try_from(n(1)?).ok()?,
                    u32::try_from(n(2)?).ok()?,
                )
            } else if parts[2].len() > 2 {
                date(
                    n(2)?,
                    u32::try_from(n(1)?).ok()?,
                    u32::try_from(n(0)?).ok()?,
                )
            } else {
                date(
                    complete_year(parts[0], n(0)?),
                    u32::try_from(n(1)?).ok()?,
                    u32::try_from(n(2)?).ok()?,
                )
            };
        }
    }

    // Month names: "Jan 5, 2024", "5 January 2024", "January 2024".
    let words: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',' || c == '-' || c == '/' || c == '.')
        .filter(|w| !w.is_empty())
        .collect();
    let month_index = words.iter().position(|w| month_number(w).is_some())?;
    let month = month_number(words[month_index])?;
    let numbers: Vec<&str> = words
        .iter()
        .enumerate()
        .filter(|(i, w)| *i != month_index && w.bytes().all(|b| b.is_ascii_digit()))
        .map(|(_, w)| *w)
        .collect();
    let (day, year) = match numbers.as_slice() {
        [y] if y.len() == 4 => (1, y.parse().ok()?),
        [a, b] if b.len() == 4 => (a.parse().ok()?, b.parse().ok()?),
        [a, b] if a.len() == 4 => (b.parse().ok()?, a.parse().ok()?),
        _ => return None,
    };
    date(year, month, day)
}

/// `String#parameterize.underscore`.
#[must_use]
pub fn parameterize_underscore(text: &str) -> String {
    let ascii: String = loco_db::search::transliterate(text)
        .chars()
        .flat_map(|c| match c {
            'ß' => vec!['s', 's'],
            'æ' => vec!['a', 'e'],
            'Æ' => vec!['A', 'E'],
            'œ' => vec!['o', 'e'],
            'Œ' => vec!['O', 'E'],
            c if c.is_ascii() => vec![c],
            _ => vec!['?'],
        })
        .collect();
    let mut out = String::new();
    let mut pending_separator = false;
    for c in ascii.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            if pending_separator && !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            pending_separator = false;
            if c == '-' && out.ends_with('-') {
                continue;
            }
            out.push(c.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    out.trim_matches('-').replace('-', "_")
}

/// The fields a submitter's values may name: one role's fields, or those of several
/// merged roles (`fetch_fields` / `fetch_roles_fields`).
fn fetch_fields<'a>(
    template_fields: &'a [Value],
    template_submitters: &[Value],
    submitter_name: Option<&str>,
    role_names: Option<&[String]>,
    for_submitter_uuid: Option<&str>,
) -> Result<Vec<&'a Value>, String> {
    let names = || {
        let names: Vec<String> = template_submitters
            .iter()
            .map(|s| {
                s.get("name")
                    .and_then(Value::as_str)
                    .map_or("nil".into(), inspect_str)
            })
            .collect();
        format!("[{}]", names.join(", "))
    };
    let find = |name: &str| {
        template_submitters
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|s| s.get("uuid").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "Unknown submitter role: {name}. Template defines {}",
                    names()
                )
            })
    };
    let uuids: Vec<String> = if let Some(roles) = role_names.filter(|r| !r.is_empty()) {
        roles.iter().map(|r| find(r)).collect::<Result<_, _>>()?
    } else {
        let role_uuid = match (submitter_name, for_submitter_uuid) {
            (Some(name), None) => Some(find(name)?),
            _ => None,
        };
        match for_submitter_uuid.map(str::to_owned).or(role_uuid) {
            Some(uuid) => vec![uuid],
            None if template_fields.is_empty() => vec![],
            None => {
                return Err(format!(
                    "Unknown submitter role: template defines {}",
                    names()
                ));
            }
        }
    };
    Ok(template_fields
        .iter()
        .filter(|f| {
            f.get("submitter_uuid")
                .and_then(Value::as_str)
                .is_some_and(|u| uuids.iter().any(|x| x == u))
        })
        .collect())
}

/// How values are matched to fields.
pub struct Target<'a> {
    pub template_fields: &'a [Value],
    pub template_submitters: &'a [Value],
    pub submitter_name: Option<&'a str>,
    pub role_names: Option<&'a [String]>,
    pub for_submitter_uuid: Option<&'a str>,
}

/// `NormalizeValues.call` with `add_fields: false`. Errors are the 422 messages.
pub fn normalize(
    target: &Target<'_>,
    values: &Map<String, Value>,
    throw_errors: bool,
) -> Result<Map<String, Value>, String> {
    let fields = fetch_fields(
        target.template_fields,
        target.template_submitters,
        target.submitter_name,
        target.role_names,
        target.for_submitter_uuid,
    )?;
    let name_of = |f: &Value| f.get("name").map(ruby_to_s).unwrap_or_default();
    let uuid_of = |f: &Value| f.get("uuid").and_then(Value::as_str).map(str::to_owned);

    // `build_fields_index`: by name, then parameterized name, then lowercase name; a later
    // grouping replaces an earlier one for the same key.
    let mut name_index: Vec<(String, Vec<&Value>)> = Vec::new();
    let mut insert_group = |key_of: &dyn Fn(&Value) -> String| {
        let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
        for f in &fields {
            let key = key_of(f);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(f),
                None => groups.push((key, vec![f])),
            }
        }
        for (key, list) in groups {
            match name_index.iter_mut().find(|(k, _)| *k == key) {
                Some((_, existing)) => *existing = list,
                None => name_index.push((key, list)),
            }
        }
    };
    insert_group(&|f| name_of(f));
    insert_group(&|f| parameterize_underscore(&name_of(f)));
    insert_group(&|f| name_of(f).to_lowercase());
    let by_name = |key: &str| {
        name_index
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, list)| list.clone())
            .filter(|l| !l.is_empty())
    };

    let mut out = Map::new();
    for (key, value) in values {
        if key.trim().is_empty() {
            continue;
        }
        let value_fields = match fields
            .iter()
            .find(|f| uuid_of(f).as_deref() == Some(key.as_str()))
        {
            Some(f) => vec![*f],
            None => match by_name(key).or_else(|| by_name(&key.to_lowercase())) {
                Some(list) => list,
                None if throw_errors => return Err(format!("Unknown field: {key}")),
                None => continue,
            },
        };
        for field in value_fields {
            let field_type = field
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if NON_PREFILLABLE_FIELD_TYPES.contains(&field_type) {
                continue;
            }
            let Some(uuid) = uuid_of(field) else { continue };
            if ATTACHMENT_TYPES.contains(&field_type) && !blank(Some(value)) {
                // Signature images and file downloads come with the signing flow (Phase 4).
                return Err(format!(
                    "Prefilling {field_type} fields is not supported yet"
                ));
            }
            out.insert(uuid, normalize_value(field, value));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn casts_like_rails() {
        let number = json!({"type": "number"});
        assert_eq!(normalize_value(&number, &json!("12")), json!(12));
        assert_eq!(normalize_value(&number, &json!("12.5")), json!(12.5));
        assert_eq!(normalize_value(&number, &json!("abc")), json!(0));
        assert_eq!(normalize_value(&number, &json!("1e3")), json!(1));
        assert_eq!(normalize_value(&number, &json!(2.0)), json!(2));
        assert_eq!(normalize_value(&number, &json!("  3 ")), json!(3));
        let text = json!({"type": "text"});
        assert_eq!(normalize_value(&text, &json!(12)), json!("12"));
        assert_eq!(normalize_value(&text, &json!(1.5)), json!("1.5"));
        assert_eq!(
            normalize_value(&text, &json!(["a", "b"])),
            json!("[\"a\", \"b\"]")
        );
        assert_eq!(
            normalize_value(&text, &json!({"a": 1})),
            json!("{\"a\" => 1}")
        );
        assert_eq!(normalize_value(&text, &json!(false)), Value::Null);
        assert_eq!(normalize_value(&text, &json!(true)), json!("true"));
        let checkbox = json!({"type": "checkbox"});
        assert_eq!(normalize_value(&checkbox, &json!("No")), json!(false));
    }

    #[test]
    fn normalizes_dates_like_rails() {
        let date = json!({"type": "date", "preferences": {"format": "DD/MM/YYYY"}});
        for (input, out) in [
            (json!("05/01/2024"), json!("2024-01-05")),
            (json!("5/1/2024"), json!("2024-01-05")),
            (json!(1_704_067_200), json!("2024-01-01")),
            (json!("2024-01-05"), json!("2024-01-05")),
            (json!("x"), json!("x")),
        ] {
            assert_eq!(normalize_value(&date, &input), out, "{input}");
        }
        let time = json!({"type": "date", "preferences": {"format": "DD/MM/YYYY HH:mm"}});
        assert_eq!(
            normalize_value(&time, &json!("05/01/2024 10:30")),
            json!("2024-01-05T10:30:00Z")
        );
        assert_eq!(
            normalize_value(&time, &json!("2024-01-05T10:00:00+02:00")),
            json!("2024-01-05T08:00:00Z")
        );
        assert_eq!(
            normalize_value(&time, &json!(1_704_067_200)),
            json!("2024-01-01T00:00:00Z")
        );
        let month = json!({"type": "date", "preferences": {"format": "MM/YYYY"}});
        assert_eq!(normalize_value(&month, &json!("01/2024")), json!("2024-01"));
        assert_eq!(normalize_value(&month, &json!("2024-03")), json!("2024-03"));
    }

    #[test]
    fn parses_dates_like_date_parse() {
        for (input, out) in [
            ("2024-01-05", "2024-01-05"),
            ("2024/01/05", "2024-01-05"),
            ("05.01.2024", "2024-01-05"),
            ("01/02/2024", "2024-02-01"),
            ("1/2/24", "2001-02-24"),
            ("20240105", "2024-01-05"),
            ("2024-1-5", "2024-01-05"),
            ("Jan 5, 2024", "2024-01-05"),
            ("5 January 2024", "2024-01-05"),
            ("January 2024", "2024-01-01"),
            ("2024-01-05T10:00:00Z", "2024-01-05"),
            ("05.01.24", "2005-01-24"),
        ] {
            assert_eq!(
                ruby_date_parse(input).map(|d| d.to_string()).as_deref(),
                Some(out),
                "{input}"
            );
        }
        assert_eq!(ruby_date_parse("13/14/2024"), None);
        assert_eq!(ruby_date_parse("abc"), None);
    }

    #[test]
    fn parameterizes() {
        assert_eq!(parameterize_underscore("Full Name"), "full_name");
        assert_eq!(
            parameterize_underscore("  Start Date (UTC) "),
            "start_date_utc"
        );
        assert_eq!(parameterize_underscore("Straße"), "strasse");
    }
}
