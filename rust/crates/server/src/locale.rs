//! The request locale, as `ApplicationController#with_locale` and `#with_browser_locale`
//! choose it.

use axum::http::{HeaderMap, header};

use crate::{auth::Auth, state::AppState};

/// `I18n.default_locale`.
pub const DEFAULT_LOCALE: &str = loco_views::i18n::DEFAULT_LOCALE;

/// `with_locale`: the account's locale when signed in (in development, `?lang=` wins),
/// else the default.
#[must_use]
pub fn app_locale(state: &AppState, auth: Option<&Auth>, lang: Option<&str>) -> String {
    let Some(auth) = auth else {
        return DEFAULT_LOCALE.to_owned();
    };
    if !state.config.production
        && let Some(lang) = lang.filter(|l| !l.is_empty() && available(l))
    {
        return lang.to_owned();
    }
    auth.account.locale.clone()
}

/// `with_browser_locale` (sign-in and password pages): when the app locale is English,
/// use `?lang=` or the `Accept-Language` header, mapping English variants other than
/// `en-US` to `en-GB` and others to their language (`de-AT` → `de`).
#[must_use]
pub fn browser_locale(current: &str, lang: Option<&str>, headers: &HeaderMap) -> String {
    if current != "en-US" && current != "en" {
        return current.to_owned();
    }

    let requested = lang.filter(|l| !l.is_empty()).map_or_else(
        || {
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .map(accept_language_prefix)
                .unwrap_or_default()
        },
        str::to_owned,
    );

    let locale = if requested.starts_with("en-") && requested != "en-US" {
        "en-GB".to_owned()
    } else {
        match requested.split('-').next().filter(|l| !l.is_empty()) {
            Some(language) => language.to_owned(),
            None => "en-GB".to_owned(),
        }
    };

    if available(&locale) {
        locale
    } else {
        "en-GB".to_owned()
    }
}

/// `BROWSER_LOCALE_REGEXP = /\A\w{2}(?:-\w{2})?/`.
fn accept_language_prefix(value: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 2 || !chars[..2].iter().all(|c| is_word(*c)) {
        return String::new();
    }
    if chars.len() >= 5 && chars[2] == '-' && chars[3..5].iter().all(|c| is_word(*c)) {
        chars[..5].iter().collect()
    } else {
        chars[..2].iter().collect()
    }
}

#[must_use]
pub fn available(locale: &str) -> bool {
    loco_views::i18n::AVAILABLE_LOCALES.contains(&locale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_LANGUAGE, value.parse().unwrap());
        headers
    }

    #[test]
    fn picks_browser_locale_like_rails() {
        let none = HeaderMap::new();
        assert_eq!(browser_locale("en", None, &none), "en-GB");
        assert_eq!(browser_locale("en", None, &accept("de-AT,de;q=0.9")), "de");
        assert_eq!(browser_locale("en", None, &accept("en-US,en")), "en");
        assert_eq!(browser_locale("en", None, &accept("en-AU")), "en-GB");
        assert_eq!(browser_locale("en", None, &accept("zz-ZZ")), "en-GB");
        assert_eq!(browser_locale("en", Some("fr"), &accept("de")), "fr");
        assert_eq!(browser_locale("de-DE", Some("fr"), &none), "de-DE");
    }
}
