//! `SubmitFormInviteController`: the parties a signer invites (`invite_by_uuid` and
//! `optional_invite_by_uuid`) before completing the form.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use loco_db::{JsonText, signing, submissions};
use serde_json::{Value, json};

use super::{
    Access, Form,
    submit::{RequestInfo, SubmitError, submit_values},
};
use crate::{
    api::Params, auth::MaybeAuth, errors::AppError, locale, request::ClientIp, session::Session,
    state::AppState,
};

/// `filter_invite_submitters`: the template parties this signer invites that are not
/// in the submission yet.
fn invite_submitters(form: &Form, key: &str) -> Vec<String> {
    form.template_submitters()
        .iter()
        .filter(|s| s.get(key).and_then(Value::as_str) == Some(form.submitter.uuid.as_str()))
        .filter_map(|s| s.get("uuid").and_then(Value::as_str))
        .filter(|uuid| !form.submitters.iter().any(|e| e.uuid == *uuid))
        .map(str::to_owned)
        .collect()
}

/// `params.require(:submission).permit(submitters: [%i[uuid email]]).fetch(:submitters, [])`
/// as `(uuid, email)` pairs.
fn submitters_attributes(submission: &Value) -> Vec<(Option<String>, Option<String>)> {
    let items: Vec<&Value> = match submission.get("submitters") {
        Some(Value::Array(items)) => items.iter().collect(),
        // A hash with numeric keys is permitted as an array (`submitters[0][uuid]=`).
        Some(Value::Object(map)) if map.keys().all(|k| k.chars().all(|c| c.is_ascii_digit())) => {
            map.values().collect()
        }
        _ => Vec::new(),
    };
    let scalar = |item: &Value, key: &str| match item.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    };
    items
        .into_iter()
        .filter(|item| item.is_object())
        .map(|item| (scalar(item, "uuid"), scalar(item, "email")))
        .collect()
}

/// `POST /s/:slug/invite`.
pub async fn create(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    MaybeAuth(auth): MaybeAuth,
    ClientIp(ip): ClientIp,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let Some(form) = Form::load(&state, &slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let access = Access {
        state: &state,
        headers: &headers,
        params: &params.0,
        current_user: auth.as_ref().map(|a| &a.user),
    };
    // `can_invite?`
    if form.submitter.declined_at.is_some()
        || form.submitter.completed_at.is_some()
        || form.submission.archived_at.is_some()
        || form.expired()
        || form.template_archived()
        || form.viewer()
        || !access.authorized(&form)
    {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }

    let required = invite_submitters(&form, "invite_by_uuid");
    let optional = invite_submitters(&form, "optional_invite_by_uuid");
    // `params.require(:submission)`
    let Some(submission_params) = params
        .get("submission")
        .filter(|v| !loco_domain::submit::blank(Some(v)))
    else {
        return Err(AppError::Status(StatusCode::BAD_REQUEST));
    };
    let attributes = submitters_attributes(submission_params);

    let request = RequestInfo {
        ip,
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        headers: headers.clone(),
        sid: Some(session.public_id()),
        user_id: auth.as_ref().map(|a| a.true_user.id),
        locale: locale::app_locale(&state, auth.as_ref(), None),
    };

    let mut tx = state.db.begin().await?;
    for uuid in required.iter().chain(&optional) {
        let Some((_, email)) = attributes
            .iter()
            .find(|(u, _)| u.as_deref() == Some(uuid.as_str()))
        else {
            continue;
        };
        let Some(email) = email
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .and_then(crate::api::create::email::normalize_email)
        else {
            continue;
        };
        signing::insert_invited_submitter(
            &mut *tx,
            form.submission.id,
            form.submitter.account_id,
            uuid,
            Some(&email),
            None,
            &loco_rails_compat::base58(14),
        )
        .await?;
        let mut extra = serde_json::Map::new();
        extra.insert("uuid".into(), json!(uuid));
        let data = crate::api::submitters::tracking_data(
            request.ip.as_deref(),
            request.user_agent.as_deref(),
            request.sid.as_deref(),
            request.user_id,
            extra,
        );
        submissions::insert_event(
            &mut *tx,
            &form.submitter,
            "invite_party",
            &JsonText(Value::Object(data)),
        )
        .await?;
    }
    signing::set_submitters_order(&mut *tx, form.submission.id, "preserved").await?;
    tx.commit().await?;

    let form = form.reload(&state).await?;
    if !required
        .iter()
        .all(|uuid| form.submitters.iter().any(|e| e.uuid == *uuid))
    {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }
    match submit_values(
        &state,
        form,
        &json!({ "completed": "true" }),
        &request,
        true,
    )
    .await?
    {
        Ok(()) => Ok(StatusCode::OK.into_response()),
        // Not rescued in Rails: the request fails.
        Err(SubmitError::Required(uuid)) => {
            Err(anyhow::anyhow!("required field {uuid} is missing").into())
        }
        Err(SubmitError::Invalid(message)) => Err(anyhow::anyhow!(message).into()),
    }
}
