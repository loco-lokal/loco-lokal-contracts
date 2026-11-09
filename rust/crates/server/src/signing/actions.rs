//! The signing form's small endpoints: `SubmitFormViewController`,
//! `SubmitFormClickEmailController`, `SubmitFormDeclineController`,
//! `SubmitFormValuesController` and `SubmitFormUploadController`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use loco_db::{JsonText, signing, submissions};
use loco_storage::{Attachment, Blob};
use serde_json::{Map, Value, json};

use super::{Access, Form, keys};
use crate::{
    api::{Params, params::FormParams, serialize, submitters::track_event},
    auth::MaybeAuth,
    errors::AppError,
    locale,
    request::{ClientIp, found},
    session::Session,
    state::AppState,
    webhooks::{self, EventRecord},
};

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

/// The request's base URL (`request.base_url`), for Active Storage service URLs.
#[must_use]
pub fn request_origin(state: &AppState, headers: &HeaderMap) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty());
    let Some(host) = host else {
        return state.default_url();
    };
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if state.config.force_ssl {
            "https"
        } else {
            "http"
        });
    format!("{scheme}://{host}")
}

/// `redirect_to path`: Rails sends the absolute URL.
#[must_use]
pub fn redirect_to(state: &AppState, headers: &HeaderMap, path: &str) -> Response {
    found(&format!("{}{path}", request_origin(state, headers)))
}

/// `attachment.as_json(only: %i[uuid created_at], methods: %i[url filename content_type])`.
#[must_use]
pub fn attachment_json(
    state: &AppState,
    origin: &str,
    attachment: &Attachment,
    blob: &Blob,
) -> Value {
    json!({
        "uuid": attachment.uuid,
        "created_at": serialize::time(attachment.created_at),
        "url": crate::disk::blob_url(state, origin, blob),
        "filename": blob.filename,
        "content_type": blob.content_type,
    })
}

/// `POST /s/:slug/view`.
pub async fn view(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(submitter) = submissions::find_submitter_by_slug(&state.db, &slug).await? else {
        return Err(not_found());
    };
    let token = params.str("v").unwrap_or_default();
    let verified = state
        .secrets
        .signed_id_verifier
        .verify(&token, Some("view_form"))
        .ok();
    if verified.as_ref().and_then(Value::as_str) != Some(submitter.slug.as_str()) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    signing::touch_opened(&state.db, submitter.id).await?;
    track_event(
        &state,
        &submitter,
        "view_form",
        ip.as_deref(),
        &headers,
        Some(&session.public_id()),
        None,
    )
    .await?;
    webhooks::enqueue(
        &state,
        "form.viewed",
        &[EventRecord {
            id: submitter.id,
            account_id: submitter.account_id,
        }],
    )
    .await;
    Ok(crate::api::ok(&json!({})))
}

/// `POST /s/:slug/click_email`.
pub async fn click_email(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(submitter) = submissions::find_submitter_by_slug(&state.db, &slug).await? else {
        return Err(not_found());
    };
    if let Some(t) = params.str("t").filter(|t| !t.is_empty())
        && t == crate::email_variables::tracking_param(
            &state.config.secret_key_base,
            &submitter.slug,
            "click_email",
        )
    {
        track_event(
            &state,
            &submitter,
            "click_email",
            ip.as_deref(),
            &headers,
            Some(&session.public_id()),
            None,
        )
        .await?;
    }
    Ok(crate::api::ok(&json!({})))
}

/// `POST /s/:slug/decline`.
pub async fn decline(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    let back = redirect_to(&state, &headers, &format!("/s/{}", form.submitter.slug));
    let declining_disabled =
        signing::account_configs(&state.db, form.account.id, &[keys::ALLOW_TO_DECLINE])
            .await?
            .iter()
            .any(|(_, v)| *v == Value::Bool(false));
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if declining_disabled
        || form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.template_archived()
        || form.viewer()
        || !access.authorized(&form)
    {
        return Ok(back);
    }

    let default_values = form
        .submitter
        .preferences
        .get("default_values")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut tx = state.db.begin().await?;
    signing::decline(&mut *tx, form.submitter.id, &JsonText(default_values)).await?;
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let mut reason = Map::new();
    reason.insert(
        "reason".into(),
        params.get("reason").cloned().unwrap_or(Value::Null),
    );
    let data = crate::api::submitters::tracking_data(
        ip.as_deref(),
        user_agent,
        Some(&session.public_id()),
        auth.as_ref().map(|a| a.true_user.id),
        reason,
    );
    submissions::insert_event(
        &mut *tx,
        &form.submitter,
        "decline_form",
        &JsonText(Value::Object(data)),
    )
    .await?;
    tx.commit().await?;

    crate::mail::declined_later(&state, form.submitter.id).await;
    webhooks::enqueue(
        &state,
        "form.declined",
        &[EventRecord {
            id: form.submitter.id,
            account_id: form.submitter.account_id,
        }],
    )
    .await;
    Ok(back)
}

/// `GET /s/:slug/values?field_uuid=&after=`.
pub async fn values(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if form.submitter.completed_at.is_some()
        || form.submitter.declined_at.is_some()
        || form.template_archived()
        || form.submission.archived_at.is_some()
        || form.expired()
        || !access.authorized(&form)
    {
        return Ok(crate::api::ok(&json!({})));
    }
    let field_uuid = params.str("field_uuid").unwrap_or_default();
    let value = form
        .submitter
        .values
        .get(&field_uuid)
        .cloned()
        .unwrap_or(Value::Null);
    let after = params
        .str("after")
        .and_then(|a| crate::api::time::parse_in_zone(&a, chrono_tz::UTC))
        .map(|t| t.naive_utc());
    let mut attachment = Value::Null;
    if let Some(uuid) = value.as_str().filter(|v| !v.trim().is_empty()) {
        let origin = request_origin(&state, &headers);
        for (a, b) in Attachment::with_blobs_for_records(
            &state.db,
            "Submitter",
            &[form.submitter.id],
            "attachments",
        )
        .await?
        {
            if a.uuid == uuid && after.is_none_or(|after| a.created_at >= after) {
                attachment = attachment_json(&state, &origin, &a, &b);
                break;
            }
        }
    }
    Ok(crate::api::ok(
        &json!({ "value": value, "attachment": attachment }),
    ))
}

/// `POST /s/:slug/upload`.
pub async fn upload(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    headers: HeaderMap,
    form_params: FormParams,
) -> Result<Response, AppError> {
    let locale = locale::app_locale(&state, auth.as_ref(), None);
    let error = |message: String| {
        crate::api::render(
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!({ "error": message }),
        )
    };
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    let params = &form_params.params;
    if form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.template_archived()
    {
        return Ok(error(state.views.t(&locale, "form_has_been_archived", &[])));
    }
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if !access.authorized(&form) {
        return Ok(error(state.views.t(
            &locale,
            "verification_required_refresh_the_page_and_pass_2fa",
            &[],
        )));
    }
    let Some(file) = form_params.file("file") else {
        return Ok(error("file param is missing".into()));
    };

    let kind = params.str("type").unwrap_or_default();
    let mut metadata = json!({});
    if kind == "initials" || kind == "signature" {
        let Some(image) = crate::images::load(&file.data, &file.content_type) else {
            return Ok(error(format!(
                "{kind} error, try to sign on another device"
            )));
        };
        if crate::images::is_blank(&image) {
            return Ok(error(format!("{kind} is empty")));
        }
        if crate::images::is_error(&image) {
            return Ok(error(format!(
                "{kind} error, try to sign on another device"
            )));
        }
        metadata = json!({
            "analyzed": true,
            "identified": true,
            "width": image.width(),
            "height": image.height(),
        });
    }

    if let Some(extension) = crate::files::dangerous_extension(&file.filename) {
        return Ok(error(format!("File type '.{extension}' is not allowed.")));
    }
    let mut blob = state
        .storage
        .create_and_upload(
            &state.db,
            file.data.clone(),
            &file.filename,
            Some(&file.content_type),
            metadata,
        )
        .await?;
    blob.ensure_uuid(&state.db).await?;
    let attachment = Attachment::create(
        &state.db,
        "Submitter",
        form.submitter.id,
        "attachments",
        &blob,
    )
    .await?;
    let origin = request_origin(&state, &headers);
    Ok(crate::api::ok(&attachment_json(
        &state,
        &origin,
        &attachment,
        &blob,
    )))
}

/// `Submitters.fetch_values_for_delegate`: the values without the party's signatures,
/// phone, verification and initials (unless they are defaults).
fn values_for_delegate(form: &Form) -> Map<String, Value> {
    let default_values = form
        .submitter
        .preferences
        .get("default_values")
        .and_then(Value::as_object);
    let drop: Vec<&str> = form
        .fields()
        .iter()
        .filter(|f| {
            f.get("submitter_uuid").and_then(Value::as_str) == Some(form.submitter.uuid.as_str())
        })
        .filter(|f| {
            matches!(
                f.get("type").and_then(Value::as_str),
                Some("signature" | "phone" | "verification" | "kba" | "initials")
            )
        })
        .filter_map(|f| f.get("uuid").and_then(Value::as_str))
        .filter(|uuid| {
            !default_values
                .and_then(|d| d.get(*uuid))
                .is_some_and(|v| loco_domain::submit::present(Some(v)))
        })
        .collect();
    form.submitter
        .values
        .as_object()
        .map(|values| {
            values
                .iter()
                .filter(|(k, _)| !drop.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// `POST /s/:slug/delegate`.
pub async fn delegate(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(not_found());
    };
    let back = redirect_to(&state, &headers, &format!("/s/{}", form.submitter.slug));
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    if form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.template_archived()
        || form.viewer()
        || !access.authorized(&form)
        || !access.pass_submitters_order(&form).await?
    {
        return Ok(back);
    }
    // `account_configs.find_by!(key: ALLOW_TO_DELEGATE_KEY, value: true)`
    let allowed = signing::account_configs(&state.db, form.account.id, &[keys::ALLOW_TO_DELEGATE])
        .await?
        .iter()
        .any(|(_, v)| *v == Value::Bool(true));
    if !allowed {
        return Err(not_found());
    }
    let Some(email) = params
        .get("email")
        .map(crate::api::create::values::ruby_to_s)
        .and_then(|e| crate::api::create::email::normalize_email(&e))
    else {
        return Ok(back);
    };

    let old_slug = form.submitter.slug.clone();
    let mut extra = Map::new();
    extra.insert(
        "old_email".into(),
        form.submitter
            .email
            .clone()
            .map_or(Value::Null, Value::String),
    );
    extra.insert("email".into(), Value::String(email.clone()));
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let data = crate::api::submitters::tracking_data(
        ip.as_deref(),
        user_agent,
        Some(&session.public_id()),
        auth.as_ref().map(|a| a.true_user.id),
        extra,
    );
    let values = values_for_delegate(&form);
    let mut tx = state.db.begin().await?;
    signing::insert_submitter_version(&mut *tx, &form.submitter).await?;
    submissions::insert_event(
        &mut *tx,
        &form.submitter,
        "delegate_form",
        &JsonText(Value::Object(data)),
    )
    .await?;
    signing::delegate(
        &mut *tx,
        form.submitter.id,
        &email,
        &loco_rails_compat::base58(14),
        &JsonText(Value::Object(values)),
    )
    .await?;
    tx.commit().await?;

    crate::mail::invitation_later(&state, form.submitter.id).await;
    Ok(redirect_to(
        &state,
        &headers,
        &format!("/s/{old_slug}/delegated"),
    ))
}
