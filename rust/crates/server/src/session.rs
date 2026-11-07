//! Browser sessions, replacing the Rails cookie store and Devise's remember-me cookie.
//!
//! The cookie holds a random token; `rust_sessions` stores its SHA-256 digest with the
//! signed-in user and a small JSON payload ([`SessionData`]). Sessions of signed-in users
//! last `SESSION_REMEMBER_DAYS` (default 730, as Devise's `remember_for` with
//! `remember_me` always on); anonymous sessions, which only carry a CSRF secret and
//! flash messages, last a day with a browser-session cookie.
//!
//! [`session_layer`] loads the session into the request extensions and saves it after the
//! handler ran, only when it changed.

use std::{
    fmt::Write as _,
    sync::{Arc, Mutex, MutexGuard},
};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, HeaderValue, header, request::Parts},
    middleware::Next,
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use loco_db::sessions::{self, SessionRow};
use loco_rails_compat::csrf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{errors::AppError, state::AppState};

pub const COOKIE_NAME: &str = "_loco_session";
const ANONYMOUS_LIFETIME_HOURS: i64 = 24;

/// Flash messages: shown on the next page that renders them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Flash {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert: Option<String>,
}

impl Flash {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notice.is_none() && self.alert.is_none()
    }
}

/// The JSON stored in `rust_sessions.data`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionData {
    /// Base64url CSRF secret (`session[:_csrf_token]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf: Option<String>,
    #[serde(default, skip_serializing_if = "Flash::is_empty")]
    pub flash: Flash,
    /// Devise's `user_return_to`: where to go after signing in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_to: Option<String>,
    /// Pretender's `impersonated_user_id` (a user uuid).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impersonated_user_uuid: Option<String>,
    /// Rack's public session id (`request.session.id`), recorded with submission events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

struct Inner {
    /// Digest of the token the request came with, if its session exists.
    loaded_digest: Option<String>,
    user_id: Option<i64>,
    auth_salt: Option<String>,
    data: SessionData,
    /// Flash taken from the stored session for this request's page.
    flash_now: Flash,
    dirty: bool,
    /// Issue a new token (sign-in, sign-out), dropping the old row.
    renew: bool,
}

/// The current request's session. Cloning shares it.
#[derive(Clone)]
pub struct Session(Arc<Mutex<Inner>>);

impl Session {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().expect("session lock")
    }

    #[must_use]
    pub fn user_id(&self) -> Option<i64> {
        self.lock().user_id
    }

    #[must_use]
    pub fn auth_salt(&self) -> Option<String> {
        self.lock().auth_salt.clone()
    }

    /// Signs a user in: a new session token (against fixation), a new CSRF secret
    /// (Devise's `clean_up_csrf_token_on_authentication`), and no impersonation.
    pub fn sign_in(&self, user_id: i64, auth_salt: &str) {
        let mut inner = self.lock();
        inner.user_id = Some(user_id);
        inner.auth_salt = Some(auth_salt.to_owned());
        inner.data.csrf = None;
        inner.data.impersonated_user_uuid = None;
        inner.renew = true;
        inner.dirty = true;
    }

    /// Clears the user and all session data, keeping only the flash about to be set.
    pub fn sign_out(&self) {
        let mut inner = self.lock();
        inner.user_id = None;
        inner.auth_salt = None;
        inner.data = SessionData::default();
        inner.renew = true;
        inner.dirty = true;
    }

    /// The CSRF secret, created on first use.
    #[must_use]
    pub fn csrf_secret(&self) -> [u8; csrf::TOKEN_LENGTH] {
        let mut inner = self.lock();
        if let Some(secret) = inner
            .data
            .csrf
            .as_deref()
            .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
            .and_then(|s| <[u8; csrf::TOKEN_LENGTH]>::try_from(s).ok())
        {
            return secret;
        }
        let secret = csrf::generate_secret();
        inner.data.csrf = Some(URL_SAFE_NO_PAD.encode(secret));
        inner.dirty = true;
        secret
    }

    /// The public session id (`request.session.id`: `SecureRandom.hex(16)`), created on
    /// first use.
    #[must_use]
    pub fn public_id(&self) -> String {
        let mut inner = self.lock();
        if let Some(sid) = inner.data.sid.clone() {
            return sid;
        }
        let mut bytes = [0_u8; 16];
        rand::fill(&mut bytes);
        let sid = bytes.iter().fold(String::with_capacity(32), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        });
        inner.data.sid = Some(sid.clone());
        inner.dirty = true;
        sid
    }

    /// A masked authenticity token for a page (`form_authenticity_token`).
    #[must_use]
    pub fn csrf_token(&self) -> String {
        csrf::masked_token(&self.csrf_secret())
    }

    /// Whether the session already has a CSRF secret (without creating one).
    #[must_use]
    pub fn has_csrf_secret(&self) -> bool {
        self.lock().data.csrf.is_some()
    }

    /// Flash for the next page (`redirect_to ..., notice:`).
    pub fn flash_notice(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        inner.data.flash.notice = Some(message.into());
        inner.dirty = true;
    }

    pub fn flash_alert(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        inner.data.flash.alert = Some(message.into());
        inner.dirty = true;
    }

    /// Flash for the page rendered by this request (`flash.now`).
    pub fn flash_now_alert(&self, message: impl Into<String>) {
        self.lock().flash_now.alert = Some(message.into());
    }

    pub fn flash_now_notice(&self, message: impl Into<String>) {
        self.lock().flash_now.notice = Some(message.into());
    }

    /// The flash to render on this page: what an earlier request set, plus `flash.now`.
    /// Flash set during this request for the *next* page is kept for it.
    #[must_use]
    pub fn flash(&self) -> Flash {
        self.lock().flash_now.clone()
    }

    pub fn set_return_to(&self, path: String) {
        let mut inner = self.lock();
        inner.data.return_to = Some(path);
        inner.dirty = true;
    }

    /// Devise's `stored_location_for`: returns and forgets the path.
    #[must_use]
    pub fn take_return_to(&self) -> Option<String> {
        let mut inner = self.lock();
        let path = inner.data.return_to.take();
        if path.is_some() {
            inner.dirty = true;
        }
        path
    }

    #[must_use]
    pub fn impersonated_user_uuid(&self) -> Option<String> {
        self.lock().data.impersonated_user_uuid.clone()
    }

    pub fn set_impersonated_user_uuid(&self, uuid: Option<String>) {
        let mut inner = self.lock();
        inner.data.impersonated_user_uuid = uuid;
        inner.dirty = true;
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Session {
    type Rejection = AppError;

    #[allow(clippy::unused_async_trait_impl)] // the trait requires an async fn
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("session layer is not installed")))
    }
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.to_owned())
        .filter(|v| !v.is_empty() && v.len() <= 128)
}

/// Loads the session before the handler and saves it afterwards.
pub async fn session_layer(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let row = match cookie_token(request.headers()) {
        Some(token) => sessions::find(&state.db, &digest(&token)).await?,
        None => None,
    };

    let (loaded_digest, user_id, auth_salt, mut data) = match row {
        Some(row) => (
            Some(row.id_digest),
            row.user_id,
            row.auth_salt,
            serde_json::from_value::<SessionData>(row.data).unwrap_or_default(),
        ),
        None => (None, None, None, SessionData::default()),
    };

    // Flash is shown once: take it out of the stored session for this request.
    let flash_now = std::mem::take(&mut data.flash);
    let dirty = !flash_now.is_empty();

    let session = Session(Arc::new(Mutex::new(Inner {
        loaded_digest,
        user_id,
        auth_salt,
        data,
        flash_now,
        dirty,
        renew: false,
    })));
    request.extensions_mut().insert(session.clone());

    let mut response = next.run(request).await;

    let save = {
        let mut inner = session.lock();
        if inner.dirty {
            let keep = inner.user_id.is_some() || inner.data != SessionData::default();
            Some((
                inner.loaded_digest.take(),
                inner.renew,
                keep,
                inner.user_id,
                inner.auth_salt.clone(),
                inner.data.clone(),
            ))
        } else {
            None
        }
    };

    if let Some((loaded_digest, renew, keep, user_id, auth_salt, data)) = save {
        let reuse = loaded_digest.as_ref().filter(|_| !renew && keep);
        if let Some(old) = loaded_digest.as_ref().filter(|_| renew || !keep) {
            sessions::delete(&state.db, old).await?;
        }

        if keep {
            let signed_in = user_id.is_some();
            let lifetime = if signed_in {
                Duration::days(remember_days())
            } else {
                Duration::hours(ANONYMOUS_LIFETIME_HOURS)
            };

            let (id_digest, new_token) = if let Some(existing) = reuse {
                (existing.clone(), None)
            } else {
                let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
                (digest(&token), Some(token))
            };

            sessions::upsert(
                &state.db,
                &SessionRow {
                    id_digest,
                    user_id,
                    auth_salt,
                    data: serde_json::to_value(&data)?,
                    expires_at: Utc::now() + lifetime,
                },
            )
            .await?;

            // A new token gets a cookie; a reused one keeps the cookie it already has (a
            // signed-in session's cookie was set with the full lifetime at sign-in).
            if let Some(token) = new_token {
                set_cookie(
                    response.headers_mut(),
                    &token,
                    signed_in.then_some(lifetime),
                    state.config.force_ssl,
                );
            }
        } else {
            clear_cookie(response.headers_mut(), state.config.force_ssl);
        }
    }

    Ok(response)
}

fn remember_days() -> i64 {
    std::env::var("SESSION_REMEMBER_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(730)
}

fn set_cookie(headers: &mut HeaderMap, token: &str, max_age: Option<Duration>, secure: bool) {
    let mut cookie = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax");
    if let Some(max_age) = max_age {
        let _ = write!(cookie, "; Max-Age={}", max_age.num_seconds());
    }
    if secure {
        cookie.push_str("; Secure");
    }
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

fn clear_cookie(headers: &mut HeaderMap, secure: bool) {
    let mut cookie = format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}
