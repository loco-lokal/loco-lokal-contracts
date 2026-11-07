//! Rendering a page with the context the layouts and shared partials expect
//! (`layouts/application`: locale, CSRF token, flash, navbar user).

use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{auth::Auth, errors::AppError, session::Session, state::AppState};

/// `Docuseal.product_name`.
pub const PRODUCT_NAME: &str = "DocuSeal";

/// `Docuseal::SOURCE_CODE_URL`.
#[must_use]
pub fn source_code_url() -> String {
    std::env::var("SOURCE_CODE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://github.com/loco-lokal/loco-lokal-contracts".to_owned())
}

pub struct Page<'a> {
    state: &'a AppState,
    session: &'a Session,
    locale: String,
    auth: Option<&'a Auth>,
    status: StatusCode,
    vars: Map<String, Value>,
}

impl<'a> Page<'a> {
    #[must_use]
    pub fn new(state: &'a AppState, session: &'a Session, locale: impl Into<String>) -> Self {
        Self {
            state,
            session,
            locale: locale.into(),
            auth: None,
            status: StatusCode::OK,
            vars: Map::new(),
        }
    }

    #[must_use]
    pub fn auth(mut self, auth: Option<&'a Auth>) -> Self {
        self.auth = auth;
        self
    }

    #[must_use]
    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Adds template variables (an object's fields).
    #[must_use]
    pub fn with(mut self, vars: impl Serialize) -> Self {
        if let Ok(Value::Object(map)) = serde_json::to_value(vars) {
            self.vars.extend(map);
        }
        self
    }

    pub fn render(self, template: &str) -> Result<Response, AppError> {
        let root_url = format!("{}/", self.state.default_url());
        let flash = self.session.flash();

        let current_user = self.auth.map(|auth| {
            json!({
                "id": auth.user.id,
                "email": auth.user.email,
                "first_name": auth.user.first_name,
                "last_name": auth.user.last_name,
                "initials": auth.user.initials(),
                "full_name": auth.user.full_name(),
            })
        });

        let mut ctx = Map::new();
        ctx.insert("locale".into(), json!(self.locale));
        ctx.insert("csrf_token".into(), json!(self.session.csrf_token()));
        ctx.insert(
            "flash".into(),
            json!({ "notice": flash.notice, "alert": flash.alert }),
        );
        ctx.insert("root_url".into(), json!(root_url));
        ctx.insert("signed_in".into(), json!(self.auth.is_some()));
        ctx.insert("current_user".into(), current_user.unwrap_or(Value::Null));
        ctx.insert(
            "impersonating".into(),
            json!(self.auth.is_some_and(Auth::impersonating)),
        );
        ctx.insert(
            "testing_account".into(),
            json!(self.auth.is_some_and(|a| a.account.testing)),
        );
        ctx.insert("product_name".into(), json!(PRODUCT_NAME));
        ctx.insert("source_code_url".into(), json!(source_code_url()));
        ctx.extend(self.vars);

        let html = self.state.views.render(template, Value::Object(ctx))?;
        Ok((self.status, Html(html)).into_response())
    }
}
