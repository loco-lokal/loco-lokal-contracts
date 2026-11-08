//! `I18n.l` for times in a Rails time zone, and `TimeUtils.timezone_abbr`.

use chrono::{NaiveDate, NaiveDateTime, Offset as _, TimeZone as _};
use chrono_tz::{OffsetName as _, Tz};

use crate::{api::time::account_zone, state::AppState};

/// `TimeUtils.timezone_abbr(timezone, time)`: the zone's abbreviation at that moment.
#[must_use]
pub fn timezone_abbr(timezone: &str, utc: NaiveDateTime) -> String {
    let tz = account_zone(timezone);
    tz.offset_from_utc_datetime(&utc)
        .abbreviation()
        .unwrap_or("UTC")
        .to_owned()
}

/// `I18n.l(utc.in_time_zone(timezone), format:, locale:)`.
#[must_use]
pub fn time(
    state: &AppState,
    locale: &str,
    utc: NaiveDateTime,
    timezone: &str,
    format: &str,
) -> String {
    let tz: Tz = account_zone(timezone);
    let offset = tz.offset_from_utc_datetime(&utc);
    let local = utc + offset.fix();
    state.views.i18n().localize_time(
        locale,
        local,
        offset.fix().local_minus_utc(),
        offset.abbreviation().unwrap_or("UTC"),
        format,
    )
}

/// `I18n.l(date, format:, locale:)`.
#[must_use]
pub fn date(state: &AppState, locale: &str, date: NaiveDate, format: &str) -> String {
    state.views.i18n().localize_date(locale, date, format)
}

/// `I18n.l(utc.in_time_zone(timezone).to_date, format:, locale:)`.
#[must_use]
pub fn local_date(
    state: &AppState,
    locale: &str,
    utc: NaiveDateTime,
    timezone: &str,
    format: &str,
) -> String {
    let tz: Tz = account_zone(timezone);
    date(
        state,
        locale,
        tz.from_utc_datetime(&utc).date_naive(),
        format,
    )
}

/// `ActiveSupport::NumberHelper::NumberToDelimitedConverter` on the integer part.
fn delimit(integer: &str, delimiter: &str) -> String {
    let (sign, digits) = match integer.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", integer),
    };
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push_str(delimiter);
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// `value.to_s` of a number value as Ruby prints it (`1.0`, `2`, `"3.5"`).
fn number_to_s(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap_or_default();
            if f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        other => crate::api::create::values::ruby_to_s(other),
    }
}

/// `number_with_delimiter(number, locale:)`: strings that are not numbers stay as given.
fn with_delimiter(state: &AppState, locale: &str, text: &str) -> String {
    let i18n = state.views.i18n();
    if text.trim().parse::<f64>().is_err() {
        return text.to_owned();
    }
    let delimiter = i18n.text(locale, "number.format.delimiter").unwrap_or(",");
    let separator = i18n.text(locale, "number.format.separator").unwrap_or(".");
    let (integer, fraction) = text.split_once('.').unwrap_or((text, ""));
    let integer = delimit(integer, delimiter);
    if fraction.is_empty() {
        integer
    } else {
        format!("{integer}{separator}{fraction}")
    }
}

/// `number_to_currency(number, locale:, precision: 2, unit:)`.
#[must_use]
pub fn currency(state: &AppState, locale: &str, text: &str, unit: &str) -> String {
    let i18n = state.views.i18n();
    let Ok(number) = text.trim().parse::<f64>() else {
        return text.to_owned();
    };
    let delimiter = i18n
        .text(locale, "number.currency.format.delimiter")
        .unwrap_or(",");
    let separator = i18n
        .text(locale, "number.currency.format.separator")
        .unwrap_or(".");
    let format = i18n
        .text(locale, "number.currency.format.format")
        .unwrap_or("%u%n");
    let rounded = format!("{:.2}", number.abs());
    let (integer, fraction) = rounded.split_once('.').unwrap_or((&rounded, "00"));
    let digits = format!("{}{separator}{fraction}", delimit(integer, delimiter));
    let formatted = format.replace("%n", &digits).replace("%u", unit);
    // `negative_format` is `-` before the format in every locale used here.
    if number < 0.0 {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// `NumberUtils.format_number(number, format)`.
#[must_use]
pub fn format_number(state: &AppState, value: &serde_json::Value, format: Option<&str>) -> String {
    let text = number_to_s(value);
    match format.unwrap_or_default() {
        "usd" => currency(state, "en", &text, "$"),
        "gbp" => currency(state, "en", &text, "£"),
        "eur" => currency(state, "fr", &text, "€"),
        "dot" => with_delimiter(state, "de", &text),
        "space" => with_delimiter(state, "fr", &text),
        "comma" => with_delimiter(state, "en", &text),
        "percent" => format!("{text}%"),
        "percent_space" => format!("{} %", text.replace('.', ",")),
        _ => text,
    }
}

/// `TimeUtils::ALL_FORMATS` over `TOKEN_REGEX`; time tokens become `--` for previews.
fn date_pattern(format: &str, preview: bool) -> String {
    const TOKENS: &[(&str, &str, bool)] = &[
        ("MMMM", "%B", false),
        ("MMM", "%b", false),
        ("MM", "%m", false),
        ("M", "%-m", false),
        ("DD", "%d", false),
        ("D", "%-d", false),
        ("YYYY", "%Y", false),
        ("YYY", "%Y", false),
        ("YY", "%y", false),
        ("HH", "%H", true),
        ("hh", "%I", true),
        ("H", "%-H", true),
        ("h", "%-I", true),
        ("mm", "%M", true),
        ("m", "%-M", true),
        ("ss", "%S", true),
        ("s", "%-S", true),
        ("A", "%p", true),
        ("a", "%P", true),
        ("z", "%Z", false),
    ];
    let mut out = String::new();
    let mut rest = format;
    'outer: while !rest.is_empty() {
        for (token, directive, is_time) in TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(if preview && *is_time { "--" } else { directive });
                rest = after;
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The format with dates upper-cased, or the locale default (`MM/DD/YYYY` for `*-US`).
fn normalized_date_format(format: Option<&str>, locale: &str) -> String {
    let format = format.unwrap_or_default();
    let format = if loco_domain::submit::format_with_time(format) {
        format.to_owned()
    } else {
        format.to_uppercase()
    };
    if format.trim().is_empty() {
        if locale.ends_with("US") {
            "MM/DD/YYYY".to_owned()
        } else {
            "DD/MM/YYYY".to_owned()
        }
    } else {
        format
    }
}

/// `TimeUtils.format_date_preview(format, locale, timezone)` for now.
#[must_use]
pub fn format_date_preview(
    state: &AppState,
    format: Option<&str>,
    locale: &str,
    timezone: &str,
) -> String {
    let format = normalized_date_format(format, locale);
    let pattern = date_pattern(&format, true);
    time(
        state,
        locale,
        chrono::Utc::now().naive_utc(),
        timezone,
        &pattern,
    )
}

/// `TimeUtils.format_date_string(string, format, locale, timezone:)`; values that do not
/// parse are returned unchanged.
#[must_use]
pub fn format_date_string(
    state: &AppState,
    value: &str,
    format: Option<&str>,
    locale: &str,
    timezone: &str,
) -> String {
    let format = normalized_date_format(format, locale);
    let pattern = date_pattern(&format, false);
    if loco_domain::submit::format_with_time(&format) {
        return match chrono::DateTime::parse_from_rfc3339(value.trim()) {
            Ok(t) => time(state, locale, t.naive_utc(), timezone, &pattern),
            Err(_) => value.to_owned(),
        };
    }
    let is_month = value.len() == 7
        && value.as_bytes()[4] == b'-'
        && value
            .chars()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit());
    let parsed = if is_month {
        let (y, m) = value.split_at(4);
        NaiveDate::from_ymd_opt(y.parse().unwrap_or(0), m[1..].parse().unwrap_or(0), 1)
    } else {
        crate::api::create::values::ruby_date_parse(value)
    };
    parsed.map_or_else(|| value.to_owned(), |d| date(state, locale, d, &pattern))
}
