//! `Params::SubmissionCreateValidator`: shape checks of `POST /api/submissions` with the
//! same messages. Shapes the Ruby validator would crash on pass, as they do in
//! production (`BaseValidator.call` logs and swallows other errors there).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{email::valid_email_list, values::blank};

static PHONE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A\+\d+\z").expect("valid regex"));

#[derive(Clone, Copy)]
enum Kind {
    String,
    Hash,
    Array,
    Integer,
}

impl Kind {
    fn matches(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Hash => value.is_object(),
            Self::Array => value.is_array(),
            Self::Integer => value.is_i64() || value.is_u64(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Hash => "Object",
            Self::Array => "Array",
            Self::Integer => "Integer",
        }
    }
}

/// `params[key]`, also for an array and an integer key.
fn get<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    match params {
        Value::Object(map) => map.get(key),
        Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    }
}

struct Validator {
    path: String,
}

type Check = Result<(), String>;

impl Validator {
    fn error(&self, message: String) -> Check {
        if self.path.is_empty() {
            Err(message)
        } else {
            Err(format!("{message} in `{}`.", self.path))
        }
    }

    fn required(&self, params: &Value, keys: &[&str], message: Option<&str>) -> Check {
        if keys.iter().any(|k| !blank(get(params, k))) {
            return Ok(());
        }
        self.error(message.map_or_else(
            || format!("{} is required", keys.join(" or ")),
            str::to_owned,
        ))
    }

    fn kind(&self, params: &Value, key: &str, kind: Kind) -> Check {
        if blank(Some(params)) {
            return Ok(());
        }
        match get(params, key) {
            Some(v) if !blank(Some(v)) && !kind.matches(v) => {
                self.error(format!("{key} must be a {}", kind.name()))
            }
            _ => Ok(()),
        }
    }

    fn email_format(&self, params: &Value, key: &str, message: &str) -> Check {
        match get(params, key) {
            Some(v) if !blank(Some(v)) => {
                let text = super::values::ruby_to_s(v);
                if valid_email_list(&text) {
                    Ok(())
                } else {
                    // Every caller passes a message, which replaces the default one.
                    self.error(message.to_owned())
                }
            }
            _ => Ok(()),
        }
    }

    fn boolean(&self, params: &Value, key: &str) -> Check {
        // `ActiveModel::Type::Boolean` casts everything but "" to true or false.
        match get(params, key) {
            Some(Value::String(s)) if s.is_empty() => {
                self.error(format!("{key} must be true or false"))
            }
            _ => Ok(()),
        }
    }

    fn order_in(&self, params: &Value) -> Check {
        match get(params, "order") {
            None | Some(Value::Null) => Ok(()),
            Some(Value::String(s)) if s == "preserved" || s == "random" => Ok(()),
            Some(_) => self.error("order must be one of preserved, random".into()),
        }
    }

    fn unique_role(&self, submitters: &Value) -> Check {
        let Value::Array(items) = submitters else {
            return Ok(());
        };
        let roles: Vec<&Value> = items
            .iter()
            .filter_map(|s| get(s, "role"))
            .filter(|r| !blank(Some(r)))
            .collect();
        let mut unique = roles.clone();
        unique.dedup_by(|a, b| a == b);
        let distinct = roles
            .iter()
            .enumerate()
            .all(|(i, r)| !roles[..i].contains(r));
        if distinct {
            Ok(())
        } else {
            self.error("role must be unique".into())
        }
    }

    /// `in_path`: runs `f` on `params[path]` with the path extended.
    fn in_path(&mut self, segment: &str, f: impl FnOnce(&mut Self) -> Check) -> Check {
        let old = self.path.clone();
        self.path = [old.as_str(), segment]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(".");
        let result = f(self);
        self.path = old;
        result
    }

    /// `in_path_each`: runs `f` on each element of the array at `path`.
    fn in_path_each(
        &mut self,
        label: &str,
        items: Option<&Value>,
        mut f: impl FnMut(&mut Self, &Value) -> Check,
    ) -> Check {
        let Some(Value::Array(items)) = items else {
            return Ok(());
        };
        for (index, item) in items.iter().enumerate() {
            if item.is_null() || item == &Value::Bool(false) {
                continue;
            }
            let segment = format!("{label}[{index}]");
            self.in_path(&segment, |v| f(v, item))?;
        }
        Ok(())
    }

    fn message(&mut self, params: &Value, skip_blank: bool) -> Check {
        let Some(message) = get(params, "message") else {
            return Ok(());
        };
        if message.is_null()
            || message == &Value::Bool(false)
            || (skip_blank && blank(Some(message)))
        {
            return Ok(());
        }
        self.in_path("message", |v| {
            v.kind(message, "subject", Kind::String)?;
            v.kind(message, "body", Kind::String)
        })
    }

    fn submitter(&mut self, s: &Value) -> Check {
        if blank(get(s, "invite_by")) {
            self.required(s, &["email", "phone", "name"], None)?;
        } else {
            self.required(s, &["role"], None)?;
        }
        self.kind(s, "name", Kind::String)?;
        self.kind(s, "reply_to", Kind::String)?;
        self.kind(s, "email", Kind::String)?;
        self.kind(s, "order", Kind::Integer)?;
        self.email_format(s, "email", "email is invalid")?;
        self.email_format(s, "reply_to", "reply_to email is invalid")?;
        self.kind(s, "phone", Kind::String)?;
        if let Some(phone) = get(s, "phone").filter(|p| !blank(Some(p)))
            && !PHONE.is_match(&super::values::ruby_to_s(phone))
        {
            return self
                .error("phone should start with +<country code> and contain only digits".into());
        }
        self.kind(s, "values", Kind::Hash)?;
        self.kind(s, "metadata", Kind::Hash)?;
        self.boolean(s, "send_email")?;
        self.boolean(s, "send_sms")?;
        self.kind(s, "completed_redirect_url", Kind::String)?;
        self.kind(s, "fields", Kind::Array)?;
        self.in_path_each("fields", get(s, "fields"), |v, field| {
            v.required(field, &["name", "uuid"], None)?;
            v.kind(field, "name", Kind::String)?;
            v.kind(field, "validation_pattern", Kind::String)?;
            v.kind(field, "invalid_message", Kind::String)?;
            v.boolean(field, "readonly")
        })
    }

    fn check_emails(&mut self, p: &Value) -> Check {
        self.required(p, &["template_id"], None)?;
        self.required(p, &["emails", "email"], None)?;
        self.kind(p, "emails", Kind::String)?;
        self.email_format(p, "emails", "emails are invalid")?;
        self.boolean(p, "send_email")?;
        self.kind(p, "message", Kind::Hash)?;
        self.message(p, false)
    }

    fn check_submitters(&mut self, p: &Value) -> Check {
        self.required(p, &["template_id"], None)?;
        self.required(p, &["submitters"], None)?;
        self.boolean(p, "send_email")?;
        self.boolean(p, "send_sms")?;
        self.kind(p, "order", Kind::String)?;
        self.kind(p, "completed_redirect_url", Kind::String)?;
        self.kind(p, "bcc_completed", Kind::String)?;
        self.kind(p, "reply_to", Kind::String)?;
        self.email_format(p, "bcc_completed", "bcc_completed email is invalid")?;
        self.email_format(p, "reply_to", "reply_to email is invalid")?;
        self.kind(p, "message", Kind::Hash)?;
        self.kind(p, "variables", Kind::Hash)?;
        self.kind(p, "submitters", Kind::Array)?;
        self.message(p, true)?;
        self.order_in(p)?;
        if let Some(submitters) = get(p, "submitters").filter(|s| !blank(Some(s))) {
            self.in_path("submitters", |v| {
                v.kind(submitters, "0", Kind::Hash)?;
                v.unique_role(submitters)
            })?;
        }
        self.in_path_each("submitters", get(p, "submitters"), Self::submitter)
    }

    fn check_submission(&mut self, p: &Value) -> Check {
        self.required(p, &["template_id"], None)?;
        self.required(p, &["submission", "submissions"], None)?;
        self.boolean(p, "send_email")?;
        self.boolean(p, "send_sms")?;
        self.kind(p, "order", Kind::String)?;
        self.kind(p, "completed_redirect_url", Kind::String)?;
        self.kind(p, "bcc_completed", Kind::String)?;
        self.email_format(p, "bcc_completed", "bcc_completed email is invalid")?;
        self.kind(p, "message", Kind::Hash)?;
        self.message(p, false)?;
        self.order_in(p)?;
        let submission = get(p, "submission");
        if submission.is_some_and(Value::is_array) {
            return Ok(());
        }
        if let Some(submission) = submission.filter(|s| !s.is_null() && *s != &Value::Bool(false)) {
            self.in_path("submission", |v| {
                v.required(submission, &["submitters"], None)
            })?;
            let submitters = get(submission, "submitters");
            self.in_path_each("submission.submitters", submitters, Self::submitter)?;
        }
        Ok(())
    }
}

/// Validates the create parameters; the error is the 422 message.
pub fn submission_create(params: &Value) -> Result<(), String> {
    let mut v = Validator {
        path: String::new(),
    };
    let has = |k: &str| get(params, k).is_some();
    if blank(get(params, "submission"))
        && (!blank(get(params, "emails")) || !blank(get(params, "email")))
    {
        v.check_emails(params)
    } else if has("submitters") {
        v.check_submitters(params)
    } else if has("submission") || has("submissions") {
        v.check_submission(params)
    } else {
        v.required(params, &["submitters"], None)?;
        v.kind(params, "submitters", Kind::Array)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reports_rails_messages() {
        assert_eq!(
            submission_create(&json!({})),
            Err("submitters is required".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "submitters": [{"role": "A"}]})),
            Err("email or phone or name is required in `submitters[0]`.".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "submitters": [{"email": "x"}]})),
            Err("email is invalid in `submitters[0]`.".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "submitters": [
                {"role": "A", "email": "a@b.com"}, {"role": "A", "email": "c@d.com"}]})),
            Err("role must be unique in `submitters`.".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "submitters": [
                {"email": "a@b.com", "fields": [{"readonly": true}]}]})),
            Err("name or uuid is required in `submitters[0].fields[0]`.".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "submission": {"submitters": [{"phone": "123"}]}})),
            Err("phone should start with +<country code> and contain only digits in `submission.submitters[0]`.".into())
        );
        assert_eq!(
            submission_create(&json!({"template_id": 1, "emails": "a@b.com"})),
            Ok(())
        );
        assert_eq!(
            submission_create(
                &json!({"template_id": 1, "submitters": [{"email": "a@b.com"}], "order": "x"})
            ),
            Err("order must be one of preserved, random".into())
        );
    }
}
