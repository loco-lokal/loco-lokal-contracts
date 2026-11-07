//! Who is signed in: Warden/Devise for pages, `X-Auth-Token` for the API, and
//! Pretender impersonation (`current_user` vs `true_user`).

use axum::{
    Json,
    extract::FromRequestParts,
    http::{HeaderMap, Method, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use chrono::{Duration, NaiveDateTime, Utc};
use loco_db::users::{self, Account, User};
use loco_domain::ability::Ability;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{errors::AppError, session::Session, state::AppState};

/// `config.maximum_attempts`.
pub const MAXIMUM_ATTEMPTS: i32 = 15;
/// `config.unlock_in`.
pub const UNLOCK_IN_HOURS: i64 = 23;

/// The signed-in user, the user they impersonate (if any), and the account.
#[derive(Clone, Debug)]
pub struct Auth {
    /// `current_user`: the impersonated user while impersonating.
    pub user: User,
    /// `true_user`: who actually signed in.
    pub true_user: User,
    /// `current_account` (the current user's account).
    pub account: Account,
}

impl Auth {
    #[must_use]
    pub fn ability(&self) -> Ability {
        Ability {
            user_id: self.user.id,
            account_id: self.account.id,
            linked_account: self.account.linked,
        }
    }

    #[must_use]
    pub fn impersonating(&self) -> bool {
        self.user.id != self.true_user.id
    }
}

/// Lockable's `access_locked?`: locked less than `unlock_in` ago.
#[must_use]
pub fn access_locked(locked_at: Option<NaiveDateTime>) -> bool {
    locked_at.is_some_and(|at| at > (Utc::now() - Duration::hours(UNLOCK_IN_HOURS)).naive_utc())
}

/// Lockable's `lock_expired?`.
#[must_use]
pub fn lock_expired(locked_at: Option<NaiveDateTime>) -> bool {
    locked_at.is_some_and(|_| !access_locked(locked_at))
}

/// `active_for_authentication?` (not archived, account not archived, not locked).
#[must_use]
pub fn active_for_authentication(user: &User, account: &Account) -> bool {
    user.archived_at.is_none() && account.archived_at.is_none() && !access_locked(user.locked_at)
}

/// The session's user, checked like Warden does on every request; an invalid session
/// (deleted user, changed password, archived or locked) is signed out.
pub async fn session_auth(state: &AppState, session: &Session) -> Result<Option<Auth>, AppError> {
    let Some(user_id) = session.user_id() else {
        return Ok(None);
    };

    let Some(true_user) = users::find(&state.db, user_id).await? else {
        session.sign_out();
        return Ok(None);
    };
    if session.auth_salt().as_deref() != Some(true_user.authenticatable_salt()) {
        session.sign_out();
        return Ok(None);
    }
    let Some(true_account) = users::find_account(&state.db, true_user.account_id).await? else {
        session.sign_out();
        return Ok(None);
    };
    if !active_for_authentication(&true_user, &true_account) {
        session.sign_out();
        return Ok(None);
    }

    let mut impersonated = None;
    if let Some(uuid) = session.impersonated_user_uuid() {
        impersonated = users::find_by_uuid(&state.db, &uuid).await?;
        if impersonated.is_none() {
            session.set_impersonated_user_uuid(None);
        }
    }

    let (user, account) = match impersonated {
        Some(user) => {
            let account = users::find_account(&state.db, user.account_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("impersonated user has no account"))?;
            (user, account)
        }
        None => (true_user.clone(), true_account),
    };

    Ok(Some(Auth {
        user,
        true_user,
        account,
    }))
}

/// Optional authentication for pages that work either way.
pub struct MaybeAuth(pub Option<Auth>);

impl FromRequestParts<AppState> for MaybeAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let session = Session::from_request_parts(parts, state).await?;
        Ok(Self(session_auth(state, &session).await?))
    }
}

/// `before_action :maybe_redirect_to_setup` + `authenticate_user!`.
pub struct RequireAuth(pub Auth);

impl FromRequestParts<AppState> for RequireAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;

        if let Some(auth) = session_auth(state, &session)
            .await
            .map_err(IntoResponse::into_response)?
        {
            return Ok(Self(auth));
        }

        if !users::any_exist(&state.db)
            .await
            .map_err(|e| AppError::from(e).into_response())?
        {
            return Err(crate::request::found("/setup"));
        }

        Err(unauthenticated(state, &session, parts))
    }
}

/// Devise's failure app for an unauthenticated request: 401 JSON for JSON requests,
/// otherwise remember the page (GET, non-XHR) and redirect to sign-in.
fn unauthenticated(state: &AppState, session: &Session, parts: &Parts) -> Response {
    let locale = crate::locale::DEFAULT_LOCALE;
    let message = state.views.t(locale, "devise.failure.unauthenticated", &[]);

    if wants_json(&parts.headers) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response();
    }

    let xhr = parts
        .headers
        .get("x-requested-with")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"XMLHttpRequest"));
    if parts.method == Method::GET && !xhr {
        let path = parts
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned();
        session.set_return_to(path);
    }

    session.flash_alert(message);
    crate::request::found("/sign_in")
}

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.starts_with("application/json"))
}

/// `Api::ApiBaseController#current_user`: the session user, else the active owner of the
/// `X-Auth-Token`. Fails with `401 {"error": "Not authenticated"}`.
pub struct ApiAuth(pub Auth);

impl FromRequestParts<AppState> for ApiAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let not_authenticated = || {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Not authenticated" })),
            )
                .into_response()
        };

        if let Ok(session) = Session::from_request_parts(parts, state).await
            && let Some(auth) = session_auth(state, &session)
                .await
                .map_err(IntoResponse::into_response)?
        {
            return Ok(Self(auth));
        }

        let Some(token) = parts
            .headers
            .get("x-auth-token")
            .and_then(|v| v.to_str().ok())
            .filter(|t| !t.trim().is_empty())
        else {
            return Err(not_authenticated());
        };

        let sha256 = hex::encode(Sha256::digest(token.as_bytes()));
        let Some(user) = users::find_by_access_token_digest(&state.db, &sha256)
            .await
            .map_err(|e| AppError::from(e).into_response())?
        else {
            return Err(not_authenticated());
        };
        let Some(account) = users::find_account(&state.db, user.account_id)
            .await
            .map_err(|e| AppError::from(e).into_response())?
        else {
            return Err(not_authenticated());
        };

        Ok(Self(Auth {
            true_user: user.clone(),
            user,
            account,
        }))
    }
}

/// `after_sign_in_path_for`: a safe local `redir` parameter, else the stored location,
/// else `/`. Absolute URLs are only followed to this app's origin, as Rails refuses
/// redirects to other hosts.
#[must_use]
pub fn after_sign_in_path(state: &AppState, session: &Session, redir: Option<&str>) -> String {
    let stored = session.take_return_to();
    redir
        .filter(|r| !r.is_empty())
        .and_then(|r| safe_redirect(state, r))
        .or(stored.as_deref().and_then(|r| safe_redirect(state, r)))
        .unwrap_or_else(|| "/".to_owned())
}

/// A same-origin path for `redirect_to`, or `None`.
#[must_use]
pub fn safe_redirect(state: &AppState, target: &str) -> Option<String> {
    if target.starts_with('/') && !target.starts_with("//") && !target.starts_with("/\\") {
        return Some(target.to_owned());
    }
    let origin = state.default_url();
    let url = url::Url::parse(target).ok()?;
    (url.origin().ascii_serialization() == origin).then(|| target.to_owned())
}
