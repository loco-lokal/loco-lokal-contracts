//! `SetupController`: first-run creation of the account and its admin user.

use axum::{Form, extract::State, http::StatusCode, response::Response};
use loco_db::users::{self, NewAccount, NewUser};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    auth::MaybeAuth,
    errors::AppError,
    locale,
    page::{PRODUCT_NAME, Page},
    request::{ClientIp, found},
    routes::hash_password,
    session::Session,
    state::{APP_URL_KEY, AppState, ESIGN_CERTS_KEY},
    validation::{Errors, valid_email, validate_password},
};

/// `AccountsController::LOCALE_OPTIONS`.
const LOCALE_OPTIONS: &[(&str, &str)] = &[
    ("en-US", "English (United States)"),
    ("en-GB", "English (United Kingdom)"),
    ("fr-FR", "Français"),
    ("es-ES", "Español"),
    ("pt-PT", "Português"),
    ("de-DE", "Deutsch"),
    ("it-IT", "Italiano"),
    ("nl-NL", "Nederlands"),
];

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SetupForm {
    #[serde(rename(deserialize = "user[first_name]"), default)]
    first_name: String,
    #[serde(rename(deserialize = "user[last_name]"), default)]
    last_name: String,
    #[serde(rename(deserialize = "user[email]"), default)]
    email: String,
    #[serde(rename(deserialize = "user[password]"), default, skip_serializing)]
    password: String,
    #[serde(rename(deserialize = "account[name]"), default)]
    account_name: String,
    #[serde(rename(deserialize = "account[timezone]"), default)]
    timezone: String,
    #[serde(rename(deserialize = "account[locale]"), default)]
    locale: String,
    #[serde(rename(deserialize = "encrypted_config[value]"), default)]
    app_url: String,
}

/// Redirects that run before both actions: signed-in users go home, and setup is closed
/// once a user exists.
async fn guard(
    state: &AppState,
    session: &Session,
    signed_in: bool,
) -> Result<Option<Response>, AppError> {
    if signed_in {
        session.flash_notice(state.views.t(
            locale::DEFAULT_LOCALE,
            "you_are_already_signed_in",
            &[],
        ));
        return Ok(Some(found("/")));
    }
    if users::any_exist(&state.db).await? {
        session.flash_notice(state.views.t(locale::DEFAULT_LOCALE, "please_sign_in", &[]));
        return Ok(Some(found("/sign_in")));
    }
    Ok(None)
}

fn render(
    state: &AppState,
    session: &Session,
    form: &SetupForm,
    status: StatusCode,
) -> Result<Response, AppError> {
    let mut form = json!(form);
    // Model defaults shown by `Account.new` (`timezone: 'UTC'`, `locale: 'en-US'`).
    if form["timezone"] == "" {
        form["timezone"] = json!("UTC");
    }
    if form["locale"] == "" {
        form["locale"] = json!("en-US");
    }

    Page::new(state, session, locale::DEFAULT_LOCALE)
        .status(status)
        .with(json!({
            "request_path": "/setup",
            "form": form,
            "locale_options": LOCALE_OPTIONS
                .iter()
                .map(|(code, label)| json!({ "code": code, "label": label }))
                .collect::<Vec<_>>(),
        }))
        .render("setup/index.html")
}

pub async fn index(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
) -> Result<Response, AppError> {
    if let Some(redirect) = guard(&state, &session, auth.is_some()).await? {
        return Ok(redirect);
    }
    render(&state, &session, &SetupForm::default(), StatusCode::OK)
}

pub async fn create(
    State(state): State<AppState>,
    session: Session,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    Form(mut form): Form<SetupForm>,
) -> Result<Response, AppError> {
    if let Some(redirect) = guard(&state, &session, auth.is_some()).await? {
        return Ok(redirect);
    }

    form.timezone = loco_domain::timezones::normalize(if form.timezone.is_empty() {
        "UTC"
    } else {
        &form.timezone
    });
    if form.locale.is_empty() {
        form.locale = "en-US".into();
    }

    let app_url_ok = url::Url::parse(form.app_url.trim())
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https"));

    let mut errors = Errors::new("user");
    let email = users::normalize_email(&form.email);
    if email.is_empty() {
        errors.add("email", "blank");
    } else if !valid_email(&email) {
        errors.add("email", "invalid");
    }
    validate_password(&mut errors, &form.password, None);

    if !app_url_ok || !errors.is_empty() {
        // Like Rails, the setup page re-renders without listing the errors.
        return render(&state, &session, &form, StatusCode::UNPROCESSABLE_ENTITY);
    }

    let encrypted_password = hash_password(form.password.clone()).await?;
    let certs =
        tokio::task::spawn_blocking(|| loco_domain::certificates::generate(PRODUCT_NAME)).await??;

    let mut tx = state.db.begin().await?;
    let account_id = users::insert_account(
        &mut *tx,
        NewAccount {
            name: &form.account_name,
            timezone: &form.timezone,
            locale: &form.locale,
        },
    )
    .await?;
    let user = users::insert(
        &mut *tx,
        NewUser {
            account_id,
            first_name: &form.first_name,
            last_name: &form.last_name,
            email: &email,
            encrypted_password: &encrypted_password,
        },
    )
    .await?;
    users::insert_encrypted_config(
        &mut *tx,
        account_id,
        APP_URL_KEY,
        &state
            .secrets
            .encryptor
            .encrypt(&json!(form.app_url).to_string()),
    )
    .await?;
    users::insert_encrypted_config(
        &mut *tx,
        account_id,
        ESIGN_CERTS_KEY,
        &state
            .secrets
            .encryptor
            .encrypt(&serde_json::to_string(&certs)?),
    )
    .await?;
    users::insert_account_config(&mut *tx, account_id, "fulltext_search", &json!(true)).await?;
    tx.commit().await?;

    state.refresh_default_url().await?;

    crate::routes::sessions::sign_in(&state, &session, &user, ip.as_deref()).await?;
    Ok(found("/"))
}
