//! The recipient signing form under `/s/:slug`: `SubmitFormController` and its small
//! companions (`SubmitForm{View,ClickEmail,Decline,Values,Upload}Controller`), with
//! `Submitters::AuthorizedForForm`, `Submitters::FormConfigs` and
//! `Submitters::SubmitValues`.

pub mod actions;
pub mod completion;
pub mod pages;
pub mod submit;

use axum::{
    Router,
    http::HeaderMap,
    routing::{get, post},
};
use chrono::Utc;
use loco_db::{
    signing,
    submissions::{self, Submission, Submitter},
    templates::{self, Template},
    users::{self, Account},
};
use loco_domain::submit::Party;
use serde_json::{Map, Value};

use crate::{errors::AppError, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/s/{slug}",
            get(pages::show).put(submit::update).patch(submit::update),
        )
        .route("/s/{slug}/completed", get(pages::completed))
        .route("/s/{slug}/view", post(actions::view))
        .route("/s/{slug}/click_email", post(actions::click_email))
        .route("/s/{slug}/decline", post(actions::decline))
        .route("/s/{slug}/values", get(actions::values))
        .route("/s/{slug}/upload", post(actions::upload))
        .route("/success", get(pages::success))
}

/// `AccountConfig` keys read by the form (`Submitters::FormConfigs::DEFAULT_KEYS`).
pub mod keys {
    pub const FORM_COMPLETED_BUTTON: &str = "form_completed_button";
    pub const FORM_COMPLETED_MESSAGE: &str = "form_completed_message";
    pub const FORM_WITH_CONFETTI: &str = "form_with_confetti";
    pub const FORM_PREFILL_SIGNATURE: &str = "form_prefill_signature";
    pub const WITH_SIGNATURE_ID: &str = "with_signature_id";
    pub const ALLOW_TO_DECLINE: &str = "allow_to_decline";
    pub const ALLOW_TO_DELEGATE: &str = "allow_to_delegate";
    pub const ENFORCE_SIGNING_ORDER: &str = "enforce_signing_order";
    pub const REQUIRE_SIGNING_REASON: &str = "require_signing_reason";
    pub const REUSE_SIGNATURE: &str = "reuse_signature";
    pub const WITH_FIELD_LABELS: &str = "with_field_labels";
    pub const ALLOW_TO_PARTIAL_DOWNLOAD: &str = "allow_to_partial_download";
    pub const ALLOW_TYPED_SIGNATURE: &str = "allow_typed_signature";
    pub const WITH_SUBMITTER_TIMEZONE: &str = "with_submitter_timezone";
    pub const WITH_TIMESTAMP_SECONDS: &str = "with_timestamp_seconds";
    pub const WITH_SIGNATURE_ID_REASON: &str = "with_signature_id_reason";
    pub const WITH_SIGNATURE_ID_COMPLETED_AT: &str = "with_signature_id_completed_at";
    pub const POLICY_LINKS: &str = "policy_links";

    pub const DEFAULT: &[&str] = &[
        FORM_COMPLETED_BUTTON,
        FORM_COMPLETED_MESSAGE,
        FORM_WITH_CONFETTI,
        FORM_PREFILL_SIGNATURE,
        WITH_SIGNATURE_ID,
        ALLOW_TO_DECLINE,
        ALLOW_TO_DELEGATE,
        ENFORCE_SIGNING_ORDER,
        REQUIRE_SIGNING_REASON,
        REUSE_SIGNATURE,
        WITH_FIELD_LABELS,
        ALLOW_TO_PARTIAL_DOWNLOAD,
        ALLOW_TYPED_SIGNATURE,
        WITH_SUBMITTER_TIMEZONE,
        WITH_TIMESTAMP_SECONDS,
        WITH_SIGNATURE_ID_REASON,
        WITH_SIGNATURE_ID_COMPLETED_AT,
        POLICY_LINKS,
    ];
}

/// `Submitters::FormConfigs.call`.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors the Rails hash")]
pub struct FormConfigs {
    pub completed_button: Value,
    pub completed_message: Value,
    pub with_typed_signature: bool,
    pub with_confetti: bool,
    pub prefill_signature: bool,
    pub reuse_signature: bool,
    pub with_decline: bool,
    pub with_delegate: bool,
    pub with_partial_download: bool,
    pub with_signature_id: bool,
    pub require_signing_reason: bool,
    pub enforce_signing_order: bool,
    pub with_submitter_timezone: bool,
    pub with_timestamp_seconds: bool,
    pub with_signature_id_reason: bool,
    pub with_signature_id_completed_at: bool,
    pub with_field_labels: bool,
    pub policy_links: Value,
}

impl FormConfigs {
    pub async fn load(state: &AppState, account_id: i64) -> Result<Self, AppError> {
        let configs = signing::account_configs(&state.db, account_id, keys::DEFAULT).await?;
        let find = |key: &str| {
            configs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let is_true = |key: &str| find(key) == Some(Value::Bool(true));
        let not_false = |key: &str| find(key) != Some(Value::Bool(false));
        Ok(Self {
            completed_button: find(keys::FORM_COMPLETED_BUTTON)
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| Value::Object(Map::new())),
            completed_message: find(keys::FORM_COMPLETED_MESSAGE)
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| Value::Object(Map::new())),
            with_typed_signature: not_false(keys::ALLOW_TYPED_SIGNATURE),
            with_confetti: is_true(keys::FORM_WITH_CONFETTI),
            prefill_signature: not_false(keys::FORM_PREFILL_SIGNATURE),
            reuse_signature: not_false(keys::REUSE_SIGNATURE),
            with_decline: not_false(keys::ALLOW_TO_DECLINE),
            with_delegate: is_true(keys::ALLOW_TO_DELEGATE),
            with_partial_download: not_false(keys::ALLOW_TO_PARTIAL_DOWNLOAD),
            with_signature_id: is_true(keys::WITH_SIGNATURE_ID),
            require_signing_reason: is_true(keys::REQUIRE_SIGNING_REASON),
            enforce_signing_order: is_true(keys::ENFORCE_SIGNING_ORDER),
            with_submitter_timezone: is_true(keys::WITH_SUBMITTER_TIMEZONE),
            with_timestamp_seconds: is_true(keys::WITH_TIMESTAMP_SECONDS),
            with_signature_id_reason: not_false(keys::WITH_SIGNATURE_ID_REASON),
            with_signature_id_completed_at: is_true(keys::WITH_SIGNATURE_ID_COMPLETED_AT),
            with_field_labels: not_false(keys::WITH_FIELD_LABELS),
            policy_links: find(keys::POLICY_LINKS).unwrap_or(Value::Null),
        })
    }
}

/// A submitter with the records the form reads.
#[derive(Clone, Debug)]
pub struct Form {
    pub submitter: Submitter,
    pub submission: Submission,
    pub template: Option<Template>,
    pub account: Account,
    /// All submitters of the submission, by id.
    pub submitters: Vec<Submitter>,
}

impl Form {
    pub async fn load(state: &AppState, slug: &str) -> Result<Option<Self>, AppError> {
        let Some(submitter) = submissions::find_submitter_by_slug(&state.db, slug).await? else {
            return Ok(None);
        };
        Self::for_submitter(state, submitter).await.map(Some)
    }

    pub async fn for_submitter(state: &AppState, submitter: Submitter) -> Result<Self, AppError> {
        let submission = submissions::find(&state.db, submitter.submission_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("submission {} missing", submitter.submission_id))?;
        let template = match submission.template_id {
            Some(id) => templates::find(&state.db, id).await?,
            None => None,
        };
        let account = users::find_account(&state.db, submission.account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("account {} missing", submission.account_id))?;
        let submitters = submissions::submitters_for(&state.db, &[submission.id]).await?;
        Ok(Self {
            submitter,
            submission,
            template,
            account,
            submitters,
        })
    }

    /// Reloads the submission and submitters after a write.
    pub async fn reload(self, state: &AppState) -> Result<Self, AppError> {
        let submitter = submissions::find_submitter(&state.db, self.submitter.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("submitter {} missing", self.submitter.id))?;
        Self::for_submitter(state, submitter).await
    }

    /// `submission.template_fields || submission.template.fields`.
    #[must_use]
    pub fn fields(&self) -> &[Value] {
        self.submission
            .template_fields
            .as_ref()
            .and_then(|f| f.as_array())
            .or_else(|| self.template.as_ref().and_then(|t| t.fields.as_array()))
            .map_or(&[], Vec::as_slice)
    }

    /// `submission.template_schema || submission.template.schema`.
    #[must_use]
    pub fn schema(&self) -> &[Value] {
        self.submission
            .template_schema
            .as_ref()
            .and_then(|f| f.as_array())
            .or_else(|| self.template.as_ref().and_then(|t| t.schema.as_array()))
            .map_or(&[], Vec::as_slice)
    }

    /// `submission.template_submitters || submission.template.submitters`.
    #[must_use]
    pub fn template_submitters(&self) -> &[Value] {
        self.submission
            .template_submitters
            .as_ref()
            .and_then(|f| f.as_array())
            .or_else(|| self.template.as_ref().and_then(|t| t.submitters.as_array()))
            .map_or(&[], Vec::as_slice)
    }

    /// `Submitter#viewer?` (only the submission's own snapshot counts).
    #[must_use]
    pub fn is_viewer(&self, submitter: &Submitter) -> bool {
        self.submission
            .template_submitters
            .as_ref()
            .and_then(|f| f.as_array())
            .is_some_and(|items| {
                items.iter().any(|s| {
                    s.get("uuid").and_then(Value::as_str) == Some(submitter.uuid.as_str())
                        && loco_domain::submit::present(s.get("is_viewer"))
                })
            })
    }

    #[must_use]
    pub fn viewer(&self) -> bool {
        self.is_viewer(&self.submitter)
    }

    /// The template's role name of a submitter.
    #[must_use]
    pub fn role(&self, uuid: &str) -> Option<&str> {
        self.template_submitters()
            .iter()
            .find(|s| s.get("uuid").and_then(Value::as_str) == Some(uuid))
            .and_then(|s| s.get("name").and_then(Value::as_str))
    }

    #[must_use]
    pub fn template_archived(&self) -> bool {
        self.template
            .as_ref()
            .is_some_and(|t| t.archived_at.is_some())
    }

    #[must_use]
    pub fn expired(&self) -> bool {
        self.submission.expired(Utc::now().naive_utc())
    }

    /// `template.preferences.dig(key)`.
    #[must_use]
    pub fn template_preference(&self, key: &str) -> Option<&Value> {
        self.template.as_ref().and_then(|t| t.preferences.get(key))
    }

    /// `submission.submitters.reduce({}) { |acc, sub| acc.merge(sub.values) }`.
    #[must_use]
    pub fn all_values(&self) -> Map<String, Value> {
        let mut values = Map::new();
        for submitter in &self.submitters {
            if let Some(map) = submitter.values.as_object() {
                values.extend(map.clone());
            }
        }
        values
    }

    /// The other parties' values merged (`merge_submitters_values` without this one).
    #[must_use]
    pub fn other_values(&self) -> Map<String, Value> {
        let mut values = Map::new();
        for submitter in self
            .submitters
            .iter()
            .filter(|s| s.uuid != self.submitter.uuid)
        {
            if let Some(map) = submitter.values.as_object() {
                values.extend(map.clone());
            }
        }
        values
    }

    #[must_use]
    pub fn current_submitter_order(&self) -> bool {
        let parties: Vec<Party<'_>> = self
            .submitters
            .iter()
            .map(|s| Party {
                uuid: &s.uuid,
                completed: s.completed_at.is_some(),
                viewer: self.is_viewer(s),
            })
            .collect();
        loco_domain::submit::current_submitter_order(
            &self.submitter.uuid,
            self.viewer(),
            self.template_submitters(),
            &parties,
        )
    }

    /// `I18n.t('option')` in the submitter's language (`metadata.lang` or the account's).
    #[must_use]
    pub fn option_word(&self, state: &AppState) -> String {
        let locale = self
            .submitter
            .metadata
            .get("lang")
            .and_then(Value::as_str)
            .unwrap_or(&self.account.locale)
            .to_owned();
        state.views.t(&locale, "option", &[])
    }
}

/// The two-factor token of a request (`params[:two_factor_token]` or the header).
fn two_factor_token(headers: &HeaderMap, params: &Value) -> Option<String> {
    params
        .get("two_factor_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get("x-two-factor-token")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

/// `Submitters::AuthorizedForForm`. The `email_2fa_slug` cookie is not read yet: a
/// verified email two-factor token is the only way through.
pub struct Access<'a> {
    pub state: &'a AppState,
    pub headers: &'a HeaderMap,
    pub params: &'a Value,
    pub current_user: Option<&'a loco_db::users::User>,
}

impl Access<'_> {
    fn verified_token(&self, expected: &str) -> bool {
        two_factor_token(self.headers, self.params).is_some_and(|token| {
            self.state
                .secrets
                .signed_id_verifier
                .verify(&token, Some("email_two_factor"))
                .ok()
                .as_ref()
                .and_then(Value::as_str)
                == Some(expected)
        })
    }

    /// `pass_email_2fa?`.
    #[must_use]
    pub fn pass_email_2fa(&self, form: &Form) -> bool {
        let required = form.template_preference("require_email_2fa") == Some(&Value::Bool(true))
            || form.submitter.preferences.get("require_email_2fa") == Some(&Value::Bool(true));
        !required || self.verified_token(&form.submitter.slug)
    }

    /// `pass_link_2fa?`.
    #[must_use]
    pub fn pass_link_2fa(&self, form: &Form) -> bool {
        if form.submission.source != "link"
            || form.template_preference("shared_link_2fa") != Some(&Value::Bool(true))
        {
            return true;
        }
        if let Some(user) = self.current_user
            && form.submitter.email.as_deref() == Some(user.email.as_str())
            && user.account_id == form.submitter.account_id
        {
            return true;
        }
        let key = format!(
            "{}:{}",
            squish(
                &form
                    .submitter
                    .email
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase()
            ),
            form.template
                .as_ref()
                .map(|t| t.slug.as_str())
                .unwrap_or_default()
        );
        self.verified_token(&key)
    }

    /// `AuthorizedForForm.call`.
    #[must_use]
    pub fn authorized(&self, form: &Form) -> bool {
        self.pass_email_2fa(form) && self.pass_link_2fa(form)
    }

    /// `pass_submitters_order?` (for `with_order: true`).
    pub async fn pass_submitters_order(&self, form: &Form) -> Result<bool, AppError> {
        if form.submission.submitters_order == "random" || form.template_submitters().len() < 2 {
            return Ok(true);
        }
        if form
            .template_preference("submitters_order")
            .and_then(Value::as_str)
            != Some("preserved")
        {
            let enforce = signing::account_configs(
                &self.state.db,
                form.submitter.account_id,
                &[keys::ENFORCE_SIGNING_ORDER],
            )
            .await?
            .iter()
            .any(|(_, v)| *v == Value::Bool(true));
            if !enforce {
                return Ok(true);
            }
        }
        Ok(form.current_submitter_order())
    }
}

/// `String#squish`.
#[must_use]
pub fn squish(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
