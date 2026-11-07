//! Route handlers, grouped like the Rails controllers they port.

pub mod api;
pub mod dashboard;
pub mod passwords;
pub mod sessions;
pub mod setup;

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;

/// Language options under the sign-in and password forms (`devise/shared/_links`):
/// the available locales except `en` and the regional ones.
pub(crate) fn lang_options(state: &AppState, locale: &str) -> Vec<serde_json::Value> {
    const EXCLUDED: &[&str] = &["en", "pt-PT", "de-DE", "fr-FR", "it-IT", "es-ES", "nl-NL"];
    loco_views::i18n::AVAILABLE_LOCALES
        .iter()
        .filter(|code| !EXCLUDED.contains(code))
        .map(|code| {
            serde_json::json!({
                "code": code,
                "label": state.views.t(locale, &format!("language_{code}"), &[]),
            })
        })
        .collect()
}

/// Hashes a password off the async runtime (bcrypt is deliberately slow).
pub(crate) async fn hash_password(password: String) -> anyhow::Result<String> {
    Ok(
        tokio::task::spawn_blocking(move || loco_rails_compat::password::hash(&password, None))
            .await??,
    )
}

pub(crate) async fn verify_password(password: String, encrypted: String) -> anyhow::Result<bool> {
    Ok(tokio::task::spawn_blocking(move || {
        loco_rails_compat::password::verify(&password, &encrypted, None)
    })
    .await?)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard::index))
        .route("/setup", get(setup::index).post(setup::create))
        .route("/sign_in", get(sessions::new).post(sessions::create))
        .route("/sign_out", axum::routing::delete(sessions::destroy))
        .route("/password/new", get(passwords::new))
        .route(
            "/password",
            post(passwords::create)
                .put(passwords::update)
                .patch(passwords::update),
        )
        .route("/password/edit", get(passwords::edit))
        .route(
            "/invitation",
            get(passwords::edit_invitation)
                .put(passwords::update_invitation)
                .patch(passwords::update_invitation),
        )
        .route("/api/user", get(api::user))
}
