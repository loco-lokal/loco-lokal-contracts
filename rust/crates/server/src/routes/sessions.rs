//! `SessionsController` (Devise sessions with devise-two-factor and lockable).

use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use loco_db::users::{self, User};
use loco_rails_compat::devise::Totp;
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::{self, MaybeAuth, after_sign_in_path},
    errors::AppError,
    locale,
    page::Page,
    request::{ClientIp, found, see_other},
    routes::{lang_options, verify_password},
    session::Session,
    state::AppState,
};

#[derive(Deserialize)]
pub struct NewParams {
    lang: Option<String>,
    redir: Option<String>,
}

#[derive(Deserialize)]
pub struct SignInForm {
    #[serde(rename = "user[email]", default)]
    email: String,
    #[serde(rename = "user[password]", default)]
    password: String,
    #[serde(rename = "user[otp_attempt]")]
    otp_attempt: Option<String>,
    redir: Option<String>,
    lang: Option<String>,
}

/// Signs `user` in: a fresh session and Devise trackable fields.
pub async fn sign_in(
    state: &AppState,
    session: &Session,
    user: &User,
    ip: Option<&str>,
) -> Result<(), AppError> {
    users::record_sign_in(&state.db, user.id, ip).await?;
    session.sign_in(user.id, user.authenticatable_salt());
    Ok(())
}

struct NewPage<'a> {
    locale: &'a str,
    lang: Option<&'a str>,
    redir: Option<&'a str>,
    email: Option<&'a str>,
}

fn render_new(
    state: &AppState,
    session: &Session,
    page: &NewPage<'_>,
    status: StatusCode,
) -> Result<Response, AppError> {
    Page::new(state, session, page.locale)
        .status(status)
        .with(json!({
            "request_path": "/sign_in",
            "canonical_url": format!("{}/sign_in", state.default_url()),
            "lang": page.lang,
            "redir": page.redir.filter(|r| !r.is_empty()),
            "email": page.email,
            "lang_options": lang_options(state, page.locale),
        }))
        .render("devise/sessions/new.html")
}

/// `require_no_authentication` and `maybe_redirect_to_setup`.
async fn redirect_if_needed(
    state: &AppState,
    session: &Session,
    signed_in: bool,
    redir: Option<&str>,
) -> Result<Option<Response>, AppError> {
    if signed_in {
        // The "already signed in" alert is suppressed (`set_flash_message` override).
        return Ok(Some(found(&after_sign_in_path(state, session, redir))));
    }
    if !users::any_exist(&state.db).await? {
        return Ok(Some(found("/setup")));
    }
    Ok(None)
}

pub async fn new(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    Query(params): Query<NewParams>,
) -> Result<Response, AppError> {
    if let Some(redirect) =
        redirect_if_needed(&state, &session, auth.is_some(), params.redir.as_deref()).await?
    {
        return Ok(redirect);
    }
    let locale = locale::browser_locale(locale::DEFAULT_LOCALE, params.lang.as_deref(), &headers);
    render_new(
        &state,
        &session,
        &NewPage {
            locale: &locale,
            lang: params.lang.as_deref(),
            redir: params.redir.as_deref(),
            email: None,
        },
        StatusCode::OK,
    )
}

enum Outcome {
    Success(Box<User>),
    /// Message key under `devise.failure`.
    Failure(&'static str),
}

/// Warden's two-factor and database strategies with lockable's `valid_for_authentication?`.
async fn authenticate(state: &AppState, form: &SignInForm) -> Result<Outcome, AppError> {
    let Some(mut user) = users::find_by_email(&state.db, &form.email).await? else {
        return Ok(Outcome::Failure("not_found_in_database"));
    };

    if auth::lock_expired(user.locked_at) {
        users::unlock(&state.db, user.id).await?;
        user.locked_at = None;
        user.failed_attempts = 0;
    }

    // devise-two-factor checks the code first, then the password; each failed check
    // counts as a failed attempt.
    if user.otp_required_for_login {
        let otp_ok = match &form.otp_attempt {
            Some(code) => consume_otp(state, &user, code).await?,
            None => false,
        };
        if !otp_ok || auth::access_locked(user.locked_at) {
            return failed_attempt(state, &user).await;
        }
    }

    let password_ok =
        verify_password(form.password.clone(), user.encrypted_password.clone()).await?;
    if !password_ok || auth::access_locked(user.locked_at) {
        return failed_attempt(state, &user).await;
    }

    let Some(account) = users::find_account(&state.db, user.account_id).await? else {
        return Ok(Outcome::Failure("invalid"));
    };
    if user.archived_at.is_some() || account.archived_at.is_some() {
        return Ok(Outcome::Failure("inactive"));
    }

    Ok(Outcome::Success(Box::new(user)))
}

async fn failed_attempt(state: &AppState, user: &User) -> Result<Outcome, AppError> {
    let (_, locked_at) =
        users::record_failed_attempt(&state.db, user.id, auth::MAXIMUM_ATTEMPTS).await?;
    // The unlock email of `unlock_strategy: :both` is not sent: the app has no unlock
    // route, so the lock simply expires after `unlock_in`.
    Ok(Outcome::Failure(if auth::access_locked(locked_at) {
        "locked"
    } else {
        "invalid"
    }))
}

/// `validate_and_consume_otp!`.
pub async fn consume_otp(state: &AppState, user: &User, code: &str) -> Result<bool, AppError> {
    let Some(secret) = user
        .otp_secret
        .as_deref()
        .and_then(|s| state.secrets.encryptor.decrypt(s).ok())
    else {
        return Ok(false);
    };
    let Some(totp) = Totp::new(&secret) else {
        return Ok(false);
    };
    let now = chrono::Utc::now().timestamp();
    let Some(step) = totp.verify(code, now, user.consumed_timestep.map(i64::from)) else {
        return Ok(false);
    };
    let Ok(step) = i32::try_from(step) else {
        return Ok(false);
    };
    Ok(users::consume_otp_timestep(&state.db, user.id, step).await?)
}

pub async fn create(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(form): Form<SignInForm>,
) -> Result<Response, AppError> {
    if let Some(redirect) =
        redirect_if_needed(&state, &session, auth.is_some(), form.redir.as_deref()).await?
    {
        return Ok(redirect);
    }
    let locale = locale::browser_locale(locale::DEFAULT_LOCALE, form.lang.as_deref(), &headers);
    let email = users::normalize_email(&form.email);

    // Ask for the code before checking anything else, as SessionsController#create does.
    if form.otp_attempt.as_deref().is_none_or(str::is_empty)
        && users::find_by_email(&state.db, &email)
            .await?
            .is_some_and(|u| u.otp_required_for_login)
    {
        return Page::new(&state, &session, &locale)
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .with(json!({
                "request_path": "/sign_in",
                "email": email,
                "password": form.password,
                "redir": form.redir.as_deref().filter(|r| !r.is_empty()),
            }))
            .render("devise/sessions/otp.html");
    }

    match authenticate(&state, &form).await? {
        Outcome::Success(user) => {
            sign_in(&state, &session, &user, ip.as_deref()).await?;
            session.flash_notice(state.views.t(&locale, "devise.sessions.signed_in", &[]));
            Ok(see_other(&after_sign_in_path(
                &state,
                &session,
                form.redir.as_deref(),
            )))
        }
        Outcome::Failure(key) => {
            let keys = state
                .views
                .t(&locale, "activerecord.attributes.user.email", &[]);
            let keys = if keys == "Email" || keys.is_empty() {
                "Email".to_owned()
            } else {
                keys
            };
            session.flash_now_alert(state.views.t(
                &locale,
                &format!("devise.failure.{key}"),
                &[("authentication_keys", keys)],
            ));
            render_new(
                &state,
                &session,
                &NewPage {
                    locale: &locale,
                    lang: form.lang.as_deref(),
                    redir: form.redir.as_deref(),
                    email: Some(&email),
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            )
        }
    }
}

/// `DELETE /sign_out`: ends every session of the user (`expire_all_remember_me_on_sign_out`).
pub async fn destroy(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
) -> Result<Response, AppError> {
    let locale = locale::app_locale(&state, auth.as_ref(), None);
    if let Some(auth) = &auth {
        users::forget_me(&state.db, auth.true_user.id).await?;
        loco_db::sessions::delete_for_user(&state.db, auth.true_user.id).await?;
    }
    session.sign_out();

    let key = if auth.is_some() {
        "devise.sessions.signed_out"
    } else {
        "devise.sessions.already_signed_out"
    };
    session.flash_notice(state.views.t(&locale, key, &[]));
    Ok(see_other("/"))
}
