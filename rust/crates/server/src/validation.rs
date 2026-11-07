//! Active Model–style validation errors with Rails' messages: `errors.messages.*` from
//! rails-i18n, attribute names from `activerecord.attributes.<model>.<attr>` or the
//! humanized attribute, joined by `errors.format`.

use loco_views::Views;
use serde::Serialize;

#[derive(Clone, Debug)]
struct Error {
    attribute: &'static str,
    /// Key under `errors.messages`, e.g. `blank`, `too_short`.
    message: &'static str,
    vars: Vec<(&'static str, String)>,
}

#[derive(Clone, Debug)]
pub struct Errors {
    model: &'static str,
    errors: Vec<Error>,
}

/// What `devise/shared/_error_messages` renders.
#[derive(Clone, Debug, Serialize)]
pub struct ErrorList {
    pub count: usize,
    pub header: String,
    pub messages: Vec<String>,
}

impl Errors {
    #[must_use]
    pub fn new(model: &'static str) -> Self {
        Self {
            model,
            errors: Vec::new(),
        }
    }

    pub fn add(&mut self, attribute: &'static str, message: &'static str) {
        self.add_with(attribute, message, Vec::new());
    }

    pub fn add_with(
        &mut self,
        attribute: &'static str,
        message: &'static str,
        vars: Vec<(&'static str, String)>,
    ) {
        self.errors.push(Error {
            attribute,
            message,
            vars,
        });
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    fn human_attribute(&self, views: &Views, locale: &str, attribute: &str) -> String {
        views
            .i18n()
            .translate(
                locale,
                &format!("activerecord.attributes.{}.{attribute}", self.model),
                &[],
            )
            .unwrap_or_else(|| humanize(attribute))
    }

    /// `errors.full_messages`.
    #[must_use]
    pub fn full_messages(&self, views: &Views, locale: &str) -> Vec<String> {
        self.errors
            .iter()
            .map(|error| {
                let attribute = self.human_attribute(views, locale, error.attribute);
                let mut vars = error.vars.clone();
                vars.push(("attribute", attribute.clone()));
                let message = views.t(locale, &format!("errors.messages.{}", error.message), &vars);
                views.t(
                    locale,
                    "errors.format",
                    &[("attribute", attribute), ("message", message)],
                )
            })
            .collect()
    }

    /// The list for `devise/shared/_error_messages`, or `None` without errors.
    #[must_use]
    pub fn for_display(&self, views: &Views, locale: &str) -> Option<ErrorList> {
        if self.is_empty() {
            return None;
        }
        let resource = views
            .i18n()
            .translate(
                locale,
                &format!("activerecord.models.{}", self.model),
                &[("count", "1".into())],
            )
            .unwrap_or_else(|| self.model.to_owned())
            .to_lowercase();
        let count = self.errors.len();
        Some(ErrorList {
            count,
            header: views.t(
                locale,
                "errors.messages.not_saved",
                &[("count", count.to_string()), ("resource", resource)],
            ),
            messages: self.full_messages(views, locale),
        })
    }
}

/// `String#humanize`: `first_name` → `First name`.
#[must_use]
pub fn humanize(attribute: &str) -> String {
    let text = attribute.trim_end_matches("_id").replace('_', " ");
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Devise validatable's password rules for a new password.
pub fn validate_password(errors: &mut Errors, password: &str, confirmation: Option<&str>) {
    let length = password.chars().count();
    if password.is_empty() {
        errors.add("password", "blank");
    } else if length < 6 {
        errors.add_with("password", "too_short", vec![("count", "6".into())]);
    } else if length > 128 {
        errors.add_with("password", "too_long", vec![("count", "128".into())]);
    }
    if let Some(confirmation) = confirmation
        && confirmation != password
    {
        errors.add_with(
            "password_confirmation",
            "confirmation",
            vec![("attribute", "Password".into())],
        );
    }
}

/// Devise's `email_regexp` (`/\A[^@\s]+@[^@\s]+\z/`) and the stricter format validation
/// on `User` (`/\A[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\z/`).
#[must_use]
pub fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.contains('@') {
        return false;
    }
    let local_ok = local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
    let Some((host, tld)) = domain.rsplit_once('.') else {
        return false;
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let tld_ok = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
    local_ok && host_ok && tld_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_emails_like_the_user_model() {
        assert!(valid_email("ana@example.test"));
        assert!(valid_email("a.b+c@sub.example.de"));
        assert!(!valid_email("ana@example"));
        assert!(!valid_email("ana@@example.test"));
        assert!(!valid_email("ana example@example.test"));
        assert!(!valid_email("@example.test"));
        assert!(!valid_email("ana@example.t"));
    }

    #[test]
    fn humanizes_attributes() {
        assert_eq!(humanize("first_name"), "First name");
        assert_eq!(humanize("reset_password_token"), "Reset password token");
    }
}
