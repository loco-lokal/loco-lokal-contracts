//! Route handlers, grouped like the Rails controllers they port.

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

/// `ErrorsController::ENTERPRISE_PATHS`: unrouted requests to these paths answer 404
/// with a message instead of the plain error.
const ENTERPRISE_PATHS: &[&str] = &[
    "/submissions/html",
    "/api/submissions/html",
    "/templates/html",
    "/api/templates/html",
    "/submissions/pdf",
    "/api/submissions/pdf",
    "/templates/pdf",
    "/api/templates/pdf",
    "/templates/doc",
    "/api/templates/doc",
    "/templates/docx",
    "/api/templates/docx",
];

/// POST to the enterprise paths (the other methods route to `show`, `update` and
/// `destroy` with these ids).
fn enterprise_routes() -> Router<AppState> {
    ENTERPRISE_PATHS.iter().fold(Router::new(), |router, path| {
        router.route(path, post(enterprise_feature))
    })
}

async fn enterprise_feature() -> axum::response::Response {
    crate::api::render(
        axum::http::StatusCode::NOT_FOUND,
        &serde_json::json!({
            "status": 404,
            "message": "This feature is not available in this edition",
        }),
    )
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
        .route("/templates_upload", post(crate::templates::uploads::create))
        .route(
            "/templates_upload/new",
            get(crate::templates::uploads::new_from_url),
        )
        .route(
            "/templates/{template_id}/documents",
            post(crate::templates::uploads::create_documents),
        )
        .route(
            "/templates/{template_id}/detect_fields",
            post(crate::templates::detect_fields::create),
        )
        .merge(enterprise_routes())
        .merge(crate::api::router())
        .route("/file/{signed_uuid}/{*filename}", get(crate::files::show))
        .route(
            "/blobs_proxy/{signed_uuid}/{*filename}",
            get(crate::files::show),
        )
        .route("/disk/{encoded_key}/{*filename}", get(crate::disk::show))
        .route(
            "/preview/{signed_key}/{id}",
            get(crate::template_preview::page_image),
        )
        .route(
            "/templates/{template_id}/preview",
            get(crate::template_preview::show),
        )
        .route(
            "/templates/{template_id}/documents_page_objects",
            get(crate::template_preview::page_objects),
        )
        .merge(crate::signing::router())
}
