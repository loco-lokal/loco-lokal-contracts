//! `PasswordsController` (Devise recoverable) and `InvitationsController`, which accepts
//! an invitation by setting the first password through a reset token.

use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{Duration, Utc};
use loco_db::users;
use loco_mailer::Email;
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::{self, MaybeAuth},
    errors::AppError,
    locale,
    page::{PRODUCT_NAME, Page},
    request::{ClientIp, found, see_other},
    routes::{hash_password, lang_options, sessions::sign_in},
    session::Session,
    state::AppState,
    validation::{Errors, validate_password},
};

/// `config.reset_password_within`.
const RESET_PASSWORD_WITHIN_HOURS: i64 = 6;
const TOKEN_COLUMN: &str = "reset_password_token";
const MINIMUM_PASSWORD_LENGTH: usize = 6;

#[derive(Deserialize)]
pub struct LangParams {
    lang: Option<String>,
}

#[derive(Deserialize)]
pub struct ResetRequestForm {
    #[serde(rename = "user[email]", default)]
    email: String,
}

#[derive(Deserialize)]
pub struct EditParams {
    reset_password_token: Option<String>,
    lang: Option<String>,
}

#[derive(Deserialize)]
pub struct ResetForm {
    #[serde(rename = "user[reset_password_token]", default)]
    reset_password_token: String,
    #[serde(rename = "user[password]", default)]
    password: String,
    #[serde(rename = "user[password_confirmation]")]
    password_confirmation: Option<String>,
}

pub async fn new(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    Query(params): Query<LangParams>,
) -> Result<Response, AppError> {
    if auth.is_some() {
        return Ok(found("/"));
    }
    let locale = locale::browser_locale(locale::DEFAULT_LOCALE, params.lang.as_deref(), &headers);
    Page::new(&state, &session, &locale)
        .with(json!({
            "request_path": "/password/new",
            "lang": params.lang,
            "lang_options": lang_options(&state, &locale),
        }))
        .render("devise/passwords/new.html")
}

/// `POST /password`: emails reset instructions. Errors are cleared outside multitenant
/// mode, so the response is the same whether or not the email exists.
pub async fn create(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    Form(form): Form<ResetRequestForm>,
) -> Result<Response, AppError> {
    if auth.is_some() {
        return Ok(found("/"));
    }
    let locale = locale::browser_locale(locale::DEFAULT_LOCALE, None, &headers);

    if let Some(user) = users::find_by_email(&state.db, &form.email).await? {
        send_reset_password_instructions(&state, &user).await?;
    }

    session.flash_notice(
        state
            .views
            .t(&locale, "devise.passwords.send_instructions", &[]),
    );
    Ok(see_other("/sign_in"))
}

/// Devise's `send_reset_password_instructions`: a new token, stored as its digest, and
/// the email in the account's locale. A delivery failure is logged rather than shown.
pub async fn send_reset_password_instructions(
    state: &AppState,
    user: &users::User,
) -> Result<(), AppError> {
    let tokens = loco_rails_compat::devise::TokenGenerator::new(&state.config.secret_key_base);
    let (raw, digest) = tokens.generate(TOKEN_COLUMN);
    users::set_reset_password_token(&state.db, user.id, &digest).await?;

    let account_locale = users::find_account(&state.db, user.account_id)
        .await?
        .map_or_else(|| locale::DEFAULT_LOCALE.to_owned(), |a| a.locale);

    let edit_url = format!(
        "{}/password/edit?{}",
        email_origin(state),
        form_urlencoded::Serializer::new(String::new())
            .append_pair("reset_password_token", &raw)
            .finish()
    );
    let html = state.views.render(
        "devise/mailer/reset_password_instructions.html",
        json!({
            "locale": account_locale,
            "first_name": user.first_name.as_deref().filter(|n| !n.trim().is_empty()),
            "edit_url": edit_url,
            "product_name": PRODUCT_NAME,
        }),
    )?;

    let email = Email {
        from: None,
        to: vec![user.email.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: None,
        subject: state.views.t(
            &account_locale,
            "devise.mailer.reset_password_instructions.subject",
            &[],
        ),
        html,
        text: None,
        attachments: Vec::new(),
        headers: Vec::new(),
    };
    if let Err(err) = state.mailer.deliver(&email).await {
        tracing::error!(%err, user_id = user.id, "sending reset password instructions failed");
    }
    Ok(())
}

/// `ApplicationMailer#default_url_options`: the app URL with `EMAIL_HOST` as host.
pub(crate) fn email_origin(state: &AppState) -> String {
    let origin = state.default_url();
    match std::env::var("EMAIL_HOST").ok().filter(|h| !h.is_empty()) {
        Some(host) => {
            let Ok(mut url) = url::Url::parse(&origin) else {
                return origin;
            };
            if url.set_host(Some(&host)).is_ok() {
                url.origin().ascii_serialization()
            } else {
                origin
            }
        }
        None => origin,
    }
}

fn render_edit(
    state: &AppState,
    session: &Session,
    locale: &str,
    token: &str,
    invitation: bool,
    errors: Option<&Errors>,
    status: StatusCode,
) -> Result<Response, AppError> {
    Page::new(state, session, locale)
        .status(status)
        .with(json!({
            "request_path": if invitation { "/invitation" } else { "/password/edit" },
            "invitation": invitation,
            "reset_password_token": token,
            "minimum_password_length": MINIMUM_PASSWORD_LENGTH,
            "errors": errors.and_then(|e| e.for_display(&state.views, locale)),
        }))
        .render("devise/passwords/edit.html")
}

fn edit_page(
    state: &AppState,
    session: &Session,
    headers: &HeaderMap,
    params: EditParams,
    invitation: bool,
) -> Result<Response, AppError> {
    let locale = if invitation {
        locale::DEFAULT_LOCALE.to_owned()
    } else {
        locale::browser_locale(locale::DEFAULT_LOCALE, params.lang.as_deref(), headers)
    };
    // `assert_reset_token_passed`.
    let Some(token) = params.reset_password_token.filter(|t| !t.is_empty()) else {
        session.flash_alert(state.views.t(&locale, "devise.passwords.no_token", &[]));
        return Ok(found("/sign_in"));
    };
    render_edit(
        state,
        session,
        &locale,
        &token,
        invitation,
        None,
        StatusCode::OK,
    )
}

pub async fn edit(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Query(params): Query<EditParams>,
) -> Result<Response, AppError> {
    edit_page(&state, &session, &headers, params, false)
}

pub async fn edit_invitation(
    State(state): State<AppState>,
    session: Session,
    headers: HeaderMap,
    Query(params): Query<EditParams>,
) -> Result<Response, AppError> {
    edit_page(&state, &session, &headers, params, true)
}

/// `reset_password_by_token`, then sign-in unless the user has two-factor enabled.
async fn reset(
    state: &AppState,
    session: &Session,
    ip: Option<&str>,
    headers: &HeaderMap,
    form: ResetForm,
    invitation: bool,
) -> Result<Response, AppError> {
    let locale = if invitation {
        locale::DEFAULT_LOCALE.to_owned()
    } else {
        locale::browser_locale(locale::DEFAULT_LOCALE, None, headers)
    };

    let tokens = loco_rails_compat::devise::TokenGenerator::new(&state.config.secret_key_base);
    let user = if form.reset_password_token.is_empty() {
        None
    } else {
        users::find_by_reset_password_token(
            &state.db,
            &tokens.digest(TOKEN_COLUMN, &form.reset_password_token),
        )
        .await?
    };

    let mut errors = Errors::new("user");
    match &user {
        None => errors.add("reset_password_token", "invalid"),
        Some(user) => {
            let valid_period = user.reset_password_sent_at.is_some_and(|sent| {
                sent >= (Utc::now() - Duration::hours(RESET_PASSWORD_WITHIN_HOURS)).naive_utc()
            });
            if valid_period {
                validate_password(
                    &mut errors,
                    &form.password,
                    form.password_confirmation.as_deref(),
                );
            } else {
                errors.add("reset_password_token", "expired");
            }
        }
    }

    let Some(user) = user.filter(|_| errors.is_empty()) else {
        return render_edit(
            state,
            session,
            &locale,
            &form.reset_password_token,
            invitation,
            Some(&errors),
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    };

    let encrypted_password = hash_password(form.password).await?;
    users::reset_password(&state.db, user.id, &encrypted_password, invitation).await?;
    // `unlock_access!` (unlock strategy :both includes email).
    users::unlock(&state.db, user.id).await?;

    let account_ok = users::find_account(&state.db, user.account_id)
        .await?
        .is_some_and(|account| {
            let mut user = user.clone();
            user.locked_at = None;
            auth::active_for_authentication(&user, &account)
        });

    if user.otp_required_for_login || !account_ok {
        session.flash_notice(
            state
                .views
                .t(&locale, "devise.passwords.updated_not_active", &[]),
        );
        return Ok(see_other("/sign_in"));
    }

    let user = users::find(&state.db, user.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user disappeared during password reset"))?;
    sign_in(state, session, &user, ip).await?;
    session.flash_notice(state.views.t(&locale, "devise.passwords.updated", &[]));

    // PasswordsController goes to the sign-in page (which forwards signed-in users home);
    // InvitationsController uses Devise's default, the after-sign-in path.
    Ok(see_other(if invitation { "/" } else { "/sign_in" }))
}

pub async fn update(
    State(state): State<AppState>,
    session: Session,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> Result<Response, AppError> {
    reset(&state, &session, ip.as_deref(), &headers, form, false).await
}

pub async fn update_invitation(
    State(state): State<AppState>,
    session: Session,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> Result<Response, AppError> {
    reset(&state, &session, ip.as_deref(), &headers, form, true).await
}
