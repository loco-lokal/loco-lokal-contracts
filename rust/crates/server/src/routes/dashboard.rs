//! `DashboardController#index` (a stub until the template list is ported).

use axum::{
    extract::{Query, State},
    response::Response,
};
use serde::Deserialize;

use crate::{
    auth::RequireAuth, errors::AppError, locale, page::Page, session::Session, state::AppState,
};

#[derive(Deserialize)]
pub struct Params {
    lang: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    session: Session,
    RequireAuth(auth): RequireAuth,
    Query(params): Query<Params>,
) -> Result<Response, AppError> {
    let locale = locale::app_locale(&state, Some(&auth), params.lang.as_deref());
    Page::new(&state, &session, locale)
        .auth(Some(&auth))
        .with(serde_json::json!({ "request_path": "/" }))
        .render("dashboard/index.html")
}
