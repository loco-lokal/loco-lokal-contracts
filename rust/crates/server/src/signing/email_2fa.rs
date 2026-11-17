//! Email one-time codes: `SubmitFormEmail2fasController` (`/submit_form_email_2fa`, the
//! signing form's `require_email_2fa`), `StartFormEmail2faSendController`
//! (`/start_form_email_2fa_send`, the share link's resend button),
//! `SendSubmitterVerificationEmailJob` with `SubmitterMailer#otp_verification_email`, and
//! `TemplateMailer#otp_verification_email` with `Submitters.send_shared_link_email_verification_code`
//! and `verify_link_otp!`.
//!
//! The bounced-email checks and the `custom_otp_email` sender only apply in multitenant
//! mode, which this server does not have. The `RateLimit` calls here are always enabled,
//! as in Rails.

use std::time::Duration as StdDuration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::Engine as _;
use chrono::{Duration, Utc};
use loco_db::{JsonText, signing, submissions, templates::Template};
use loco_jobs::{EnqueueOptions, Job};
use loco_mailer::Email;
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{actions::redirect_to, pages::form_locale, squish, start_form};
use crate::{
    api::{Params, templates::blank},
    auth::MaybeAuth,
    errors::AppError,
    mail::{apply_direction, record_delivery_for},
    otp,
    page::PRODUCT_NAME,
    rate_limit::LimitApproached,
    request::ClientIp,
    session::Session,
    state::AppState,
};

/// `Submitters::StartForm::COOKIES_TTL`.
pub const COOKIES_TTL: Duration = Duration::hours(12);
/// The `RateLimit` window of these endpoints.
const RATE_TTL: StdDuration = StdDuration::from_secs(45);
const OTP_TAG: &str = "otp_verification_email";

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

/// `[email.downcase.squish, slug].join(':')`.
#[must_use]
pub fn otp_value(email: &str, slug: &str) -> String {
    format!("{}:{slug}", squish(&email.to_lowercase()))
}

/// Sets `cookies.encrypted[:email_2fa_slug]` for `COOKIES_TTL`.
pub fn set_email_2fa_cookie(
    state: &AppState,
    headers: &HeaderMap,
    response: &mut Response,
    slug: &str,
) {
    crate::cookies::set_encrypted(
        state,
        headers,
        response,
        start_form::EMAIL_2FA_COOKIE,
        slug,
        Utc::now() + COOKIES_TTL,
    );
}

/// `SubmissionEvents.create_with_tracking_data(submitter, 'email_verified', request, data)`.
pub async fn track_email_verified(
    state: &AppState,
    submitter: &submissions::Submitter,
    ip: Option<&str>,
    headers: &HeaderMap,
    session: &Session,
    user_id: Option<i64>,
    extra: Map<String, Value>,
) -> Result<(), AppError> {
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let data =
        crate::api::submitters::tracking_data(ip, ua, Some(&session.public_id()), user_id, extra);
    submissions::insert_event(
        &state.db,
        submitter,
        "email_verified",
        &JsonText(Value::Object(data)),
    )
    .await?;
    Ok(())
}

async fn load_submitter(
    state: &AppState,
    params: &Params,
) -> Result<submissions::Submitter, AppError> {
    let slug = params
        .get("submitter_slug")
        .map(crate::api::create::values::ruby_to_s)
        .unwrap_or_default();
    submissions::find_submitter_by_slug(&state.db, &slug)
        .await?
        .ok_or_else(not_found)
}

fn redirect_with_status(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    status: &str,
) -> Response {
    redirect_to(state, headers, &format!("/s/{slug}?status={status}"))
}

/// `POST /submit_form_email_2fa`: checks the code of the signing form.
pub async fn verify(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let submitter = load_submitter(&state, &params).await?;
    let slug = submitter.slug.clone();
    let too_many = || {
        session.flash_alert(state.views.t(&locale, "too_many_attempts", &[]));
        Ok(redirect_with_status(&state, &headers, &slug, "error"))
    };
    if state
        .rate_limit
        .call(&format!("verify-2fa-code-{}", submitter.id), 2, RATE_TTL)
        .is_err()
    {
        return too_many();
    }
    let Some(email) = submitter.email.as_deref() else {
        // `nil.downcase` raises in Rails.
        return Err(AppError::Internal(anyhow::anyhow!(
            "submitter {} has no email",
            submitter.id
        )));
    };
    let code = otp::digits(
        &params
            .get("one_time_code")
            .map(crate::api::create::values::ruby_to_s)
            .unwrap_or_default(),
    );
    match otp::verify(&state, &code, &otp_value(email, &slug), otp::FORM_EMAIL_2FA).await? {
        Err(LimitApproached) => too_many(),
        Ok(true) => {
            let mut extra = Map::new();
            extra.insert("email".into(), Value::String(email.to_owned()));
            track_email_verified(
                &state,
                &submitter,
                ip.as_deref(),
                &headers,
                &session,
                auth.as_ref().map(|a| a.true_user.id),
                extra,
            )
            .await?;
            let mut response = redirect_to(&state, &headers, &format!("/s/{slug}"));
            set_email_2fa_cookie(&state, &headers, &mut response, &slug);
            Ok(response)
        }
        Ok(false) => {
            session.flash_alert(state.views.t(&locale, "invalid_code", &[]));
            Ok(redirect_with_status(&state, &headers, &slug, "error"))
        }
    }
}

/// `PUT /submit_form_email_2fa`: sends the code of the signing form.
pub async fn send(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let submitter = load_submitter(&state, &params).await?;
    let slug = submitter.slug.clone();
    let since = (Utc::now() - Duration::seconds(15)).naive_utc();
    if signing::has_event_since(&state.db, submitter.id, "send_2fa_email", since).await? {
        session.flash_alert(state.views.t(&locale, "rate_limit_exceeded", &[]));
        return Ok(redirect_with_status(&state, &headers, &slug, "error"));
    }
    if state
        .rate_limit
        .call(&format!("send-email-code-{}", submitter.id), 2, RATE_TTL)
        .is_err()
    {
        session.flash_alert(state.views.t(&locale, "too_many_attempts", &[]));
        return Ok(redirect_with_status(&state, &headers, &slug, "error"));
    }
    loco_jobs::enqueue(
        &state.db,
        &SendSubmitterVerificationEmail {
            submitter_id: submitter.id,
            locale: locale.clone(),
        },
        EnqueueOptions::default(),
    )
    .await?;
    if params.get("resend").is_some_and(|v| !blank(v)) {
        session.flash_alert(state.views.t(&locale, "code_has_been_resent", &[]));
    }
    Ok(redirect_with_status(&state, &headers, &slug, "sent"))
}

/// `SendSubmitterVerificationEmailJob`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendSubmitterVerificationEmail {
    pub submitter_id: i64,
    pub locale: String,
}

impl Job for SendSubmitterVerificationEmail {
    const KIND: &'static str = "send_submitter_verification_email";
    const QUEUE: &'static str = "default";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        send_submitter_code(&state, &self).await
    }
}

async fn send_submitter_code(
    state: &AppState,
    job: &SendSubmitterVerificationEmail,
) -> anyhow::Result<()> {
    let Some(submitter) = submissions::find_submitter(&state.db, job.submitter_id).await? else {
        anyhow::bail!("submitter {} not found", job.submitter_id);
    };
    let form = super::Form::for_submitter(state, submitter)
        .await
        .map_err(AppError::into_anyhow)?;
    let Some(email) = form.submitter.email.clone() else {
        anyhow::bail!("submitter {} has no email", job.submitter_id);
    };
    let locale = if job.locale.trim().is_empty() {
        form.account.locale.clone()
    } else {
        job.locale.clone()
    };
    let code = otp::generate(
        state,
        &otp_value(&email, &form.submitter.slug),
        otp::FORM_EMAIL_2FA,
    )
    .await
    .map_err(AppError::into_anyhow)?;
    let name = form
        .submission
        .name
        .clone()
        .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default();
    deliver_code(
        state,
        &locale,
        "submitter_mailer/otp_verification_email.html",
        &name,
        &code,
        &email,
        (form.account.id, "Submitter", form.submitter.id),
    )
    .await?;
    submissions::insert_event(
        &state.db,
        &form.submitter,
        "send_2fa_email",
        &JsonText(json!({ "email": email })),
    )
    .await?;
    Ok(())
}

/// `TemplateMailer.otp_verification_email(template, email:).deliver_later!`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendTemplateVerificationEmail {
    pub template_id: i64,
    pub email: String,
    pub locale: String,
}

impl Job for SendTemplateVerificationEmail {
    const KIND: &'static str = "send_template_verification_email";
    const QUEUE: &'static str = "mailers";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        let Some(template) = loco_db::templates::find(&state.db, self.template_id).await? else {
            anyhow::bail!("template {} not found", self.template_id);
        };
        let code = otp::generate(
            &state,
            &otp_value(&self.email, &template.slug),
            otp::FORM_EMAIL_2FA,
        )
        .await
        .map_err(AppError::into_anyhow)?;
        deliver_code(
            &state,
            &self.locale,
            "template_mailer/otp_verification_email.html",
            &template.name,
            &code,
            &self.email,
            (template.account_id, "Template", template.id),
        )
        .await
    }
}

/// Sends an `otp_verification_email` from the default sender and records its
/// `email_events` row.
async fn deliver_code(
    state: &AppState,
    locale: &str,
    view: &str,
    name: &str,
    code: &str,
    to: &str,
    (account_id, emailable_type, emailable_id): (i64, &str, i64),
) -> anyhow::Result<()> {
    let html = apply_direction(state.views.render(
        view,
        json!({
            "locale": locale,
            "product_name": PRODUCT_NAME,
            "name": name,
            "otp_code": code,
        }),
    )?);
    let message_uuid = uuid::Uuid::new_v4().to_string();
    let email = Email {
        from: None,
        to: vec![to.to_owned()],
        subject: state.views.t(locale, "email_verification", &[]),
        html,
        headers: vec![("X-Message-Uuid".to_owned(), message_uuid.clone())],
        ..Email::default()
    };
    let delivered = state.mailer.deliver(&email).await?;
    record_delivery_for(
        state,
        account_id,
        OTP_TAG,
        &message_uuid,
        (emailable_type, emailable_id),
        &delivered,
    )
    .await;
    Ok(())
}

/// Why `send_shared_link_email_verification_code` sent nothing (`UnableToSendCode`).
pub struct UnableToSendCode(pub String);

/// `Submitters.send_shared_link_email_verification_code`.
pub async fn send_link_code(
    state: &AppState,
    locale: &str,
    template: &Template,
    email: &str,
    ip: Option<&str>,
) -> Result<Result<(), UnableToSendCode>, AppError> {
    if email.matches('@').count() > 1 {
        return Ok(Err(UnableToSendCode(state.views.t(
            locale,
            "provide_your_email",
            &[],
        ))));
    }
    if state
        .rate_limit
        .call(
            &format!("send-otp-code-{}", ip.unwrap_or_default()),
            2,
            RATE_TTL,
        )
        .is_err()
    {
        return Ok(Err(UnableToSendCode(state.views.t(
            locale,
            "too_many_attempts",
            &[],
        ))));
    }
    loco_jobs::enqueue(
        &state.db,
        &SendTemplateVerificationEmail {
            template_id: template.id,
            email: email.to_owned(),
            locale: locale.to_owned(),
        },
        EnqueueOptions::default(),
    )
    .await?;
    Ok(Ok(()))
}

/// What `Submitters.verify_link_otp!` concluded about a share link code.
pub enum LinkOtp {
    /// No code was sent.
    Blank,
    Verified,
    /// `InvalidOtp`.
    Invalid,
    /// `RateLimit::LimitApproached`.
    TooManyAttempts,
}

/// `Submitters.verify_link_otp!(otp, submitter)` for a submitter of `template`.
pub async fn verify_link_otp(
    state: &AppState,
    code: Option<&Value>,
    email: &str,
    template: &Template,
) -> Result<LinkOtp, AppError> {
    let Some(code) = code.filter(|v| !blank(v)) else {
        return Ok(LinkOtp::Blank);
    };
    let digest = base64::engine::general_purpose::STANDARD
        .encode(Md5::digest(squish(&email.to_lowercase())));
    if state
        .rate_limit
        .call(&format!("verify-2fa-code-{digest}"), 2, RATE_TTL)
        .is_err()
    {
        return Ok(LinkOtp::TooManyAttempts);
    }
    let code = otp::digits(&crate::api::create::values::ruby_to_s(code));
    Ok(
        match otp::verify(
            state,
            &code,
            &otp_value(email, &template.slug),
            otp::FORM_EMAIL_2FA,
        )
        .await?
        {
            Err(LimitApproached) => LinkOtp::TooManyAttempts,
            Ok(true) => LinkOtp::Verified,
            Ok(false) => LinkOtp::Invalid,
        },
    )
}

/// `submitter_params`: the permitted scalar `submitter[name|email|phone]` values, in the
/// order of `Hash#to_query` once `email_verification` is added.
fn permitted(params: &Value) -> Vec<(&'static str, Option<String>)> {
    ["email", "name", "phone"]
        .into_iter()
        .filter_map(|key| {
            let value = match params.get(key)? {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => return Some((key, None)),
                _ => return None,
            };
            Some((
                key,
                if key == "email" {
                    crate::api::create::email::normalize_email(&value)
                } else {
                    Some(value)
                },
            ))
        })
        .collect()
}

/// `start_form_path(slug, params: submitter_params.merge(email_verification: true))`.
#[must_use]
pub fn verification_path(template_slug: &str, submitter_params: &Value) -> String {
    let mut pairs: Vec<(&str, String)> = permitted(submitter_params)
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
    pairs.push(("email_verification", "true".to_owned()));
    let mut pairs: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", start_form::cgi_escape(v)))
        .collect();
    pairs.sort();
    let query = pairs.join("&");
    format!("/d/{template_slug}?{query}")
}

/// `POST /start_form_email_2fa_send`: (re)sends the share link code.
pub async fn send_link(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    let slug = params
        .get("slug")
        .map(crate::api::create::values::ruby_to_s)
        .unwrap_or_default();
    let template = loco_db::templates::find_by_slug(&state.db, &slug)
        .await?
        .ok_or_else(not_found)?;
    let account = loco_db::users::find_account(&state.db, template.account_id)
        .await?
        .ok_or_else(not_found)?;
    let back = || redirect_to(&state, &headers, &format!("/d/{}", template.slug));

    // `authorize_start!`
    if template.archived_at.is_some() || account.archived_at.is_some() {
        return Ok(back());
    }
    let submitter_params = params.get("submitter").cloned().unwrap_or(Value::Null);
    let email = submitter_params
        .get("email")
        .map(crate::api::create::values::ruby_to_s)
        .and_then(|e| crate::api::create::email::normalize_email(&e));
    let may_start =
        template.shared_link || start_form::can_read(&state, auth.as_ref(), &template).await?;
    let wants_code = template.preferences.get("shared_link_2fa") == Some(&Value::Bool(true))
        || match email.as_deref() {
            Some(email) => {
                signing::find_start_form_submitter(&state.db, template.id, Some(email), None, true)
                    .await?
                    .is_some()
            }
            None => false,
        };
    if !(may_start && wants_code) {
        return Ok(back());
    }

    // `params.require(:submitter)`
    if !submitter_params.is_object() || blank(&submitter_params) {
        return Err(AppError::Status(StatusCode::BAD_REQUEST));
    }
    let Some(email) = email else {
        session.flash_alert(state.views.t(&locale, "provide_your_email_to_start", &[]));
        return Ok(back());
    };
    let path = verification_path(&template.slug, &submitter_params);
    match send_link_code(&state, &locale, &template, &email, ip.as_deref()).await? {
        Ok(()) => {
            if params.get("resend").is_some_and(|v| !blank(v)) {
                session.flash_notice(state.views.t(&locale, "code_has_been_resent", &[]));
            }
        }
        Err(UnableToSendCode(message)) => session.flash_alert(message),
    }
    Ok(redirect_to(&state, &headers, &path))
}

/// `TextUtils.mask_value(text, unmask_size, '*')` for a positive `unmask_size`.
fn mask_value(text: &str, unmask: usize) -> String {
    let mask = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_whitespace() || "-_[]()+?.,".contains(c) {
                    c
                } else {
                    '*'
                }
            })
            .collect::<String>()
    };
    if unmask == 0 || unmask >= text.chars().count() {
        return mask(text);
    }
    let head: String = text.chars().take(unmask).collect();
    let tail: String = text.chars().skip(unmask).collect();
    head + &mask(&tail)
}

/// `TextUtils.mask_email(email, 2)`.
#[must_use]
pub fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return email.to_owned();
    };
    if local.trim().is_empty() || domain.trim().is_empty() {
        return email.to_owned();
    }
    // `String#split` drops trailing empty parts.
    let mut parts: Vec<&str> = domain.split('.').collect();
    while parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    let masked_domain = match parts.first() {
        Some(first) if !first.trim().is_empty() => {
            let mut parts: Vec<String> = parts.iter().map(|p| (*p).to_owned()).collect();
            parts[0] = mask_value(first, 1);
            parts.join(".")
        }
        _ => mask_value(domain, 1),
    };
    format!("{}@{masked_domain}", mask_value(local, 2))
}

/// `submit_form/email_2fa`: the signing form behind `require_email_2fa`.
pub(super) fn email_2fa_page(
    state: &AppState,
    session: &Session,
    locale: &str,
    form: &super::Form,
    status: Option<&str>,
    tracking: Option<&str>,
) -> Result<Response, AppError> {
    let name = form
        .submission
        .name
        .clone()
        .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default();
    // `submitters.completed.order(:completed_at).last`
    let last = form
        .submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| (s.completed_at, s.id));
    let signed_text = last.map(|last| {
        let with_signature_fields = form.fields().iter().any(|f| {
            f.get("submitter_uuid").and_then(Value::as_str) == Some(last.uuid.as_str())
                && matches!(
                    f.get("type").and_then(Value::as_str),
                    Some("signature" | "initials")
                )
        });
        let time = crate::l10n::local_date(
            state,
            locale,
            last.completed_at.unwrap_or_default(),
            &form.account.timezone,
            "long",
        );
        state.views.t(
            locale,
            if with_signature_fields {
                "signed_on_time"
            } else {
                "completed_on_time"
            },
            &[("time", time)],
        )
    });
    let masked_email = form
        .submitter
        .email
        .as_deref()
        .filter(|e| !e.trim().is_empty())
        .map(mask_email);
    crate::page::Page::new(state, session, locale)
        .with(json!({
            "robots_noindex": true,
            "html_title": format!("{name} | Loco Lokal"),
            "html_description": state.views.t(
                &form.account.locale,
                "account_name_has_invited_you_to_fill_and_sign_documents_online_effortlessly_with_a_secure_fast_and_user_friendly_digital_document_signing_solution",
                &[("account_name", form.account.name.clone())],
            ),
            "name": name,
            "signed_text": signed_text,
            "masked_email": masked_email,
            "with_code_form": matches!(status, Some("sent" | "error")),
            "tracking_param": tracking,
            "slug": form.submitter.slug,
        }))
        .render("submit_form/email_2fa.html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_verification_paths() {
        assert_eq!(
            verification_path("abc", &json!({ "email": "A@Example.com", "name": "" })),
            "/d/abc?email=a%40example.com&email_verification=true&name="
        );
        assert_eq!(
            verification_path("abc", &json!({ "phone": "+1 555", "name": ["x"] })),
            "/d/abc?email_verification=true&phone=%2B1+555"
        );
    }

    #[test]
    fn masks_emails() {
        assert_eq!(mask_email("john.doe@example.com"), "jo**.***@e******.com");
        assert_eq!(mask_email("ab@x.io"), "**@*.io");
        assert_eq!(mask_email("nobody"), "nobody");
    }

    #[test]
    fn builds_otp_values() {
        assert_eq!(otp_value(" A@B.com ", "slug"), "a@b.com:slug");
    }
}
