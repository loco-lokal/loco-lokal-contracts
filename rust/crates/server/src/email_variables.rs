//! `ReplaceEmailVariables`: `{{submitter.name}}`-style variables of email subjects and
//! bodies (`html_escape: false`, the `click_email` link).

use std::{collections::HashMap, sync::LazyLock};

use base64::Engine;
use loco_db::{
    submissions::{self, Submission, Submitter},
    templates::{self, Template},
    users::{self, Account, User},
};
use loco_storage::Attachment;
use regex::{Captures, Regex};
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::{
    api::create::values::{blank, ruby_to_s},
    errors::AppError,
    files,
    routes::passwords::email_origin,
    state::AppState,
};

const ESCAPED_OPEN: char = '\u{E000}';
const ESCAPED_CLOSE: char = '\u{E001}';

fn var(pattern: &str) -> Regex {
    Regex::new(&format!(r"(?i)\{{+{pattern}\}}+")).expect("valid regex")
}

macro_rules! vars {
    ($($name:ident = $pattern:literal;)*) => {
        $(static $name: LazyLock<Regex> = LazyLock::new(|| var($pattern));)*
    };
}

vars! {
    TEMPLATE_NAME = r"template\.name";
    SUBMISSION_NAME = r"submission\.name";
    TEMPLATE_ID = r"template\.id";
    SUBMITTER_LINK = r"submitter\.link";
    ACCOUNT_NAME = r"account\.name";
    SENDER_NAME = r"sender\.name";
    SENDER_FIRST_NAME = r"sender\.first_name";
    SENDER_EMAIL = r"sender\.email";
    SUBMITTER_EMAIL = r"submitter\.email";
    SUBMITTER_NAME = r"submitter\.name";
    SUBMITTER_FIRST_NAME = r"submitter\.first_name";
    SUBMITTER_ID = r"submitter\.id";
    SUBMITTER_SLUG = r"submitter\.slug";
    SUBMITTER_FIELD_VALUE = r"submitter\.(?<field_name>[^}]+)";
    SUBMISSION_LINK = r"submission\.link";
    SUBMISSION_ID = r"submission\.id";
    SUBMISSION_EXPIRE_AT = r"submission\.expire_at";
    SUBMITTERS = r"(?:submission\.)?submitters";
    SUBMITTERS_N_EMAIL = r"submitters\[(?<index>\d+)\]\.email";
    SUBMITTERS_N_NAME = r"submitters\[(?<index>\d+)\]\.name";
    SUBMITTERS_N_FIRST_NAME = r"submitters\[(?<index>\d+)\]\.first_name";
    SUBMITTERS_N_FIELD_VALUE = r"submitters\[(?<index>\d+)\]\.(?<field_name>[^}]+)";
    DOCUMENTS_LINKS = r"documents\.links";
    DOCUMENTS_LINK = r"documents\.link";
}

static PLUS_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+[0-9A-Za-z_]+@").expect("valid regex"));

const ATTACHMENT_TYPES: &[&str] = &["image", "signature", "initials", "stamp", "payment", "file"];

/// Does the body link the recipient to the form itself (`SUBMITTER_LINK`, `SUBMITTER_ID`,
/// `SUBMISSION_LINK` or `TEMPLATE_ID`)?
#[must_use]
pub fn links_to_form(text: &str) -> bool {
    [
        &*SUBMITTER_LINK,
        &*SUBMITTER_ID,
        &*SUBMISSION_LINK,
        &*TEMPLATE_ID,
    ]
    .iter()
    .any(|re| re.is_match(text))
}

/// `sub(/\+\w+@/, '@')`.
#[must_use]
pub fn plus_address_removed(email: &str) -> String {
    PLUS_ADDRESS.replace(email, "@").into_owned()
}

/// `SubmissionEvents.build_tracking_param`.
#[must_use]
pub fn tracking_param(secret_key_base: &str, slug: &str, event_type: &str) -> String {
    let digest = Sha1::digest(format!("{slug}:{event_type}:{secret_key_base}").as_bytes());
    base64::engine::general_purpose::URL_SAFE
        .encode(digest)
        .chars()
        .take(6)
        .collect()
}

/// The form link of an email (`submit_form_url` with `EMAIL_HOST` and the tracking param).
#[must_use]
pub fn submitter_link(state: &AppState, slug: &str) -> String {
    format!(
        "{}/s/{slug}?t={}",
        email_origin(state),
        tracking_param(&state.config.secret_key_base, slug, "click_email")
    )
}

/// `build_url_options_for(submitter, is_email: true)`: with `EMAIL_HOST` the protocol is
/// `https` only when `FORCE_SSL` is set, and the port is dropped.
fn email_host_origin(state: &AppState) -> String {
    match std::env::var("EMAIL_HOST").ok().filter(|h| !h.is_empty()) {
        Some(host) => {
            let ssl = std::env::var("FORCE_SSL").is_ok_and(|v| !v.is_empty());
            format!("{}://{host}", if ssl { "https" } else { "http" })
        }
        None => state.default_url(),
    }
}

/// `TextUtils.mask_value`: `MASK_REGEXP` characters become `X`, except for `unmask` ones
/// at the start (positive) or end (negative).
pub(crate) fn mask_value(text: &str, unmask: &Value) -> String {
    let mask = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_whitespace() || "-_[]()+?.,".contains(c) {
                    c
                } else {
                    'X'
                }
            })
            .collect::<String>()
    };
    let len = text.chars().count();
    let Some(n) = unmask
        .as_i64()
        .filter(|n| *n != 0 && n.unsigned_abs() < len as u64)
    else {
        return mask(text);
    };
    let n_abs = usize::try_from(n.unsigned_abs()).unwrap_or(len);
    let split = if n < 0 { len - n_abs } else { n_abs };
    let head: String = text.chars().take(split).collect();
    let tail: String = text.chars().skip(split).collect();
    if n < 0 {
        mask(&head) + &tail
    } else {
        head + &mask(&tail)
    }
}

/// The records the variables read, loaded for one recipient.
pub struct Vars {
    pub submitter: Submitter,
    pub submission: Submission,
    pub template: Option<Template>,
    pub account: Account,
    pub created_by: Option<User>,
    submitters: Vec<Submitter>,
    /// Signed blob URLs of the recipients' attachments by `(submitter id, attachment uuid)`.
    attachment_urls: Option<HashMap<(i64, String), String>>,
}

impl Vars {
    /// Loads the submission, template, account, creator and recipients of `submitter`.
    pub async fn load(state: &AppState, submitter: Submitter) -> Result<Option<Self>, AppError> {
        let Some(submission) = submissions::find(&state.db, submitter.submission_id).await? else {
            return Ok(None);
        };
        let Some(account) = users::find_account(&state.db, submission.account_id).await? else {
            return Ok(None);
        };
        let template = match submitter.template_id.or(submission.template_id) {
            Some(id) => templates::find(&state.db, id).await?,
            None => None,
        };
        let created_by = match submission.created_by_user_id {
            Some(id) => users::find(&state.db, id).await?,
            None => None,
        };
        let submitters = submissions::submitters_for(&state.db, &[submission.id]).await?;
        Ok(Some(Self {
            submitter,
            submission,
            template,
            account,
            created_by,
            submitters,
            attachment_urls: None,
        }))
    }

    /// The submission's recipients.
    #[must_use]
    pub fn submitters(&self) -> &[Submitter] {
        &self.submitters
    }

    #[must_use]
    pub fn template_submitters(&self) -> &[Value] {
        self.submission
            .template_submitters
            .as_deref()
            .or(self.template.as_ref().map(|t| &*t.submitters))
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn fields(&self) -> &[Value] {
        self.submission
            .template_fields
            .as_deref()
            .or(self.template.as_ref().map(|t| &*t.fields))
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    /// `Submitter#viewer?`.
    #[must_use]
    pub fn viewer(&self) -> bool {
        self.submission
            .template_submitters
            .as_deref()
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter().any(|s| {
                    s.get("uuid").and_then(Value::as_str) == Some(self.submitter.uuid.as_str())
                        && !matches!(
                            s.get("is_viewer"),
                            None | Some(Value::Null | Value::Bool(false))
                        )
                })
            })
    }

    /// `Submitter#with_signature_fields?`.
    #[must_use]
    pub fn with_signature_fields(&self) -> bool {
        self.fields().iter().any(|f| {
            f.get("submitter_uuid").and_then(Value::as_str) == Some(self.submitter.uuid.as_str())
                && matches!(
                    f.get("type").and_then(Value::as_str),
                    Some("signature" | "initials")
                )
        })
    }

    /// `submission.name || submission.template&.name`.
    #[must_use]
    pub fn submission_name(&self) -> String {
        self.submission
            .name
            .clone()
            .or_else(|| self.template.as_ref().map(|t| t.name.clone()))
            .unwrap_or_default()
    }

    /// Signs the attachment URLs, when `text` has field variables that may need them.
    async fn load_attachment_urls(&mut self, state: &AppState, text: &str) -> Result<(), AppError> {
        let needed =
            SUBMITTERS_N_FIELD_VALUE.is_match(text) || SUBMITTER_FIELD_VALUE.is_match(text);
        if !needed || self.attachment_urls.is_some() {
            return Ok(());
        }
        let mut urls = HashMap::new();
        let has_attachment_fields = self.fields().iter().any(|f| {
            f.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| ATTACHMENT_TYPES.contains(&t))
        });
        if has_attachment_fields {
            let ids: Vec<i64> = self.submitters.iter().map(|s| s.id).collect();
            let expires_at = files::link_expires_at(state, self.submission.account_id).await?;
            let attachments =
                Attachment::with_blobs_for_records(&state.db, "Submitter", &ids, "attachments")
                    .await?;
            for (attachment, mut blob) in attachments {
                let url = files::proxy_url(state, &mut blob, expires_at).await?;
                urls.insert((attachment.record_id, attachment.uuid), url);
            }
        }
        self.attachment_urls = Some(urls);
        Ok(())
    }

    /// `build_submitters_n_field`; a negative `index` counts from the end, as in Ruby.
    fn submitters_n_field(
        &self,
        index: i64,
        field: &str,
        value_name: Option<&str>,
    ) -> Option<String> {
        let list = self.template_submitters();
        let i = if index < 0 {
            i64::try_from(list.len()).ok()? + index
        } else {
            index
        };
        let uuid = list.get(usize::try_from(i).ok()?)?.get("uuid")?.as_str()?;
        let submitter = self.submitters.iter().find(|s| s.uuid == uuid)?;
        let Some(value_name) = value_name else {
            return match field {
                "name" => submitter.name.clone(),
                "email" => submitter.email.clone(),
                _ => submitter.first_name().map(str::to_owned),
            };
        };
        let field = self
            .fields()
            .iter()
            .find(|f| f.get("name").and_then(Value::as_str) == Some(value_name))?;
        let field_uuid = field
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let value = submitter.values.get(field_uuid);
        let field_type = field
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if ATTACHMENT_TYPES.contains(&field_type) {
            let attachment_uuid = match value? {
                Value::Array(items) => ruby_to_s(items.first()?),
                other => ruby_to_s(other),
            };
            return self
                .attachment_urls
                .as_ref()?
                .get(&(submitter.id, attachment_uuid))
                .cloned();
        }
        if let Some(mask) = field
            .pointer("/preferences/mask")
            .filter(|m| !blank(Some(m)))
        {
            return Some(mask_value(&value.map(ruby_to_s).unwrap_or_default(), mask));
        }
        value.map(ruby_to_s)
    }

    /// `I18n.l(expire_at.in_time_zone(account.timezone), format: :short)`.
    fn expire_at(&self, state: &AppState) -> Option<String> {
        let at = self.submission.expire_at?;
        Some(crate::l10n::time(
            state,
            &self.account.locale,
            at,
            &self.account.timezone,
            "short",
        ))
    }

    /// `submission.submitters.order(:completed_at)` names, unique, comma-separated.
    fn submitters_list(&self) -> String {
        let mut list: Vec<&Submitter> = self.submitters.iter().collect();
        // PostgreSQL sorts NULLs last in ascending order.
        list.sort_by_key(|s| (s.completed_at.is_none(), s.completed_at));
        let mut names: Vec<String> = Vec::new();
        for s in list {
            let name = s
                .name
                .clone()
                .or_else(|| s.email.clone())
                .or_else(|| s.phone.clone())
                .unwrap_or_default();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names.join(", ")
    }

    /// `ReplaceEmailVariables.call(text, submitter:)`.
    pub async fn replace(&mut self, state: &AppState, text: &str) -> Result<String, AppError> {
        self.load_attachment_urls(state, text).await?;
        Ok(self.replace_loaded(state, text))
    }

    fn replace_loaded(&self, state: &AppState, text: &str) -> String {
        let s = &self.submitter;
        let submission = &self.submission;
        let index = |c: &Captures<'_>| c["index"].parse::<i64>().map_or(-1, |i| i - 1);
        let mut text = text.to_owned();
        text = sub(&text, &TEMPLATE_NAME, |_| {
            Some(self.template.as_ref().map_or_else(
                || submission.name.clone().unwrap_or_default(),
                |t| t.name.clone(),
            ))
        });
        text = sub(&text, &SUBMISSION_NAME, |_| {
            submission
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| self.template.as_ref().map(|t| t.name.clone()))
        });
        text = sub(&text, &TEMPLATE_ID, |_| {
            self.template.as_ref().map(|t| t.id.to_string())
        });
        text = sub(&text, &SUBMITTER_ID, |_| Some(s.id.to_string()));
        text = sub(&text, &SUBMITTER_SLUG, |_| Some(s.slug.clone()));
        text = sub(&text, &SUBMISSION_ID, |_| Some(submission.id.to_string()));
        text = sub(&text, &SUBMITTER_EMAIL, |_| s.email.clone());
        text = sub(&text, &SUBMITTER_NAME, |_| {
            s.name
                .clone()
                .or_else(|| s.email.clone())
                .or_else(|| s.phone.clone())
        });
        text = sub(&text, &SUBMITTER_FIRST_NAME, |_| {
            s.first_name().map(str::to_owned)
        });
        text = sub(&text, &SUBMITTER_LINK, |_| {
            Some(submitter_link(state, &s.slug))
        });
        text = sub(&text, &SUBMISSION_LINK, |_| {
            Some(format!(
                "{}/submissions/{}",
                state.default_url(),
                submission.id
            ))
        });
        text = sub(&text, &SUBMITTERS, |_| Some(self.submitters_list()));
        let documents = |_: &Captures<'_>| {
            Some(format!(
                "{}/e/{}",
                email_host_origin(state),
                submission.slug
            ))
        };
        text = sub(&text, &DOCUMENTS_LINKS, documents);
        text = sub(&text, &DOCUMENTS_LINK, documents);
        text = sub(&text, &ACCOUNT_NAME, |_| Some(self.account.name.clone()));
        text = sub(&text, &SENDER_NAME, |_| {
            self.created_by.as_ref().map(User::full_name)
        });
        text = sub(&text, &SENDER_FIRST_NAME, |_| {
            self.created_by.as_ref().and_then(|u| u.first_name.clone())
        });
        text = sub(&text, &SUBMISSION_EXPIRE_AT, |_| self.expire_at(state));
        text = sub(&text, &SUBMITTERS_N_NAME, |c| {
            self.submitters_n_field(index(c), "name", None)
        });
        text = sub(&text, &SUBMITTERS_N_EMAIL, |c| {
            self.submitters_n_field(index(c), "email", None)
        });
        text = sub(&text, &SUBMITTERS_N_FIRST_NAME, |c| {
            self.submitters_n_field(index(c), "first_name", None)
        });
        text = sub(&text, &SUBMITTERS_N_FIELD_VALUE, |c| {
            self.submitters_n_field(index(c), "values", Some(c["field_name"].trim()))
        });
        text = sub(&text, &SUBMITTER_FIELD_VALUE, |c| {
            let position = self
                .template_submitters()
                .iter()
                .position(|e| e.get("uuid").and_then(Value::as_str) == Some(s.uuid.as_str()))?;
            self.submitters_n_field(
                i64::try_from(position).ok()?,
                "values",
                Some(c["field_name"].trim()),
            )
        });
        text = sub(&text, &SENDER_EMAIL, |_| {
            Some(
                self.created_by
                    .as_ref()
                    .map(|u| plus_address_removed(&u.email))
                    .unwrap_or_default(),
            )
        });
        text.replace(ESCAPED_OPEN, "{").replace(ESCAPED_CLOSE, "}")
    }
}

/// `replace`: braces of the value are escaped so that later variables don't match them.
fn sub(text: &str, re: &Regex, value: impl Fn(&Captures<'_>) -> Option<String>) -> String {
    re.replace_all(text, |c: &Captures<'_>| {
        value(c)
            .unwrap_or_default()
            .replace('{', &ESCAPED_OPEN.to_string())
            .replace('}', &ESCAPED_CLOSE.to_string())
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_values() {
        assert_eq!(mask_value("1234-5678", &Value::Bool(true)), "XXXX-XXXX");
        assert_eq!(mask_value("123456", &Value::from(-2)), "XXXX56");
        assert_eq!(mask_value("123456", &Value::from(2)), "12XXXX");
        assert_eq!(mask_value("12", &Value::from(5)), "XX");
    }

    #[test]
    fn detects_form_links() {
        assert!(links_to_form("Open {{submitter.link}}"));
        assert!(links_to_form("{{Template.ID}}"));
        assert!(!links_to_form("Hello {{submitter.name}}"));
    }

    #[test]
    fn removes_plus_addressing() {
        assert_eq!(plus_address_removed("a+tag@x.test"), "a@x.test");
        assert_eq!(plus_address_removed("a@x.test"), "a@x.test");
    }

    #[test]
    fn tracking_param_is_six_url_safe_chars() {
        let t = tracking_param("secret", "slug", "click_email");
        assert_eq!(t.len(), 6);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }
}
