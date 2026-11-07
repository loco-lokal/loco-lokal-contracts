//! `Api::SubmittersController` and `Api::SubmitterFormViewsController`.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use loco_db::{
    JsonText, search,
    submissions::{self, Submitter, SubmitterFilter},
};
use serde_json::json;

use super::{
    Params, access_denied, like_pattern, ok, page, pagination,
    serialize::{self, Load, SubmissionData, SubmitterOpts, Urls},
    time::parse_in_zone,
};
use crate::{
    auth::{ApiAuth, Auth},
    errors::AppError,
    request::ClientIp,
    state::AppState,
    webhooks::{self, EventRecord},
};

pub use super::create::update;

/// The submitter, or the response for a missing or forbidden one.
pub(super) async fn authorized_submitter(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    id: &str,
) -> Result<Result<Submitter, Response>, AppError> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(submitter) = submissions::find_submitter(&state.db, id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if auth.ability().can_account_record(submitter.account_id) {
        Ok(Ok(submitter))
    } else {
        Ok(Err(access_denied(
            state,
            auth,
            headers,
            "Submitter",
            submitter.id,
            Some(submitter.account_id),
        )
        .await?))
    }
}

/// Loads the submissions of `submitters` for serializing them.
pub(super) async fn load_for_submitters(
    state: &AppState,
    submitters: &[Submitter],
    load: Load,
) -> Result<(Vec<submissions::Submission>, SubmissionData), AppError> {
    let mut list = Vec::new();
    let mut by_submission: HashMap<i64, Vec<Submitter>> = HashMap::new();
    for submitter in submitters {
        if !by_submission.contains_key(&submitter.submission_id)
            && let Some(submission) = submissions::find(&state.db, submitter.submission_id).await?
        {
            list.push(submission);
        }
        by_submission
            .entry(submitter.submission_id)
            .or_default()
            .push(submitter.clone());
    }
    let data = SubmissionData::with_submitters(state, &list, by_submission, load).await?;
    Ok((list, data))
}

fn utc_param(params: &Params, key: &str) -> Option<chrono::NaiveDateTime> {
    params
        .str(key)
        .and_then(|v| parse_in_zone(&v, chrono_tz::Tz::UTC))
        .map(|t| t.naive_utc())
}

/// `GET /api/submitters`.
pub async fn index(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    params: Params,
) -> Result<Response, AppError> {
    let empty = || Ok(ok(&json!({ "data": [], "pagination": pagination(&[]) })));
    let parse_id = |key: &str| params.str(key).map(|v| v.parse::<i64>());
    let submission_id = match parse_id("submission_id") {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) => return empty(),
        None => None,
    };
    let template_id = match parse_id("template_id") {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) => return empty(),
        None => None,
    };
    let application_key = params.str("application_key");
    let external_id = params.str("external_id");
    if let (Some(a), Some(b)) = (&application_key, &external_id)
        && a != b
    {
        return empty();
    }

    let (search, search_ids) = match params.str("q") {
        None => (None, None),
        Some(q) if search::enabled(&state.db).await? => (
            None,
            Some(
                search::matching_ids(&state.db, "Submitter", &[auth.account.id], &q, false).await?,
            ),
        ),
        Some(q) => (Some(like_pattern(&q)), None),
    };
    let slug = params.str("slug");
    let filter = SubmitterFilter {
        account_id: auth.account.id,
        search_like: search.as_deref(),
        ids: search_ids.as_deref(),
        external_id: application_key.as_deref().or(external_id.as_deref()),
        slug: slug.as_deref(),
        submission_id,
        template_id,
        completed_after: utc_param(&params, "completed_after"),
        completed_before: utc_param(&params, "completed_before"),
    };
    let list = submissions::list_submitters(&state.db, &filter, page(&params)).await?;

    let load = Load {
        events: true,
        documents: true,
        attachments: true,
    };
    let (submission_list, data) = load_for_submitters(&state, &list, load).await?;
    let urls = Urls::new(&state, auth.account.id).await?;
    let opts = SubmitterOpts {
        with_template: true,
        with_events: true,
        include_fields: params.str("include").is_some_and(|i| i.contains("fields")),
        ..SubmitterOpts::default()
    };
    let items: Vec<_> = list
        .iter()
        .filter_map(|s| {
            let submission = submission_list.iter().find(|x| x.id == s.submission_id)?;
            Some(serialize::submitter_json(&urls, &data, submission, s, opts))
        })
        .collect();
    let ids: Vec<i64> = list.iter().map(|s| s.id).collect();
    Ok(ok(
        &json!({ "data": items, "pagination": pagination(&ids) }),
    ))
}

/// `GET /api/submitters/:id`.
pub async fn show(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submitter = match authorized_submitter(&state, &auth, &headers, &id).await? {
        Ok(submitter) => submitter,
        Err(response) => return Ok(response),
    };
    let load = Load {
        events: true,
        documents: true,
        attachments: true,
    };
    let (list, data) = load_for_submitters(&state, std::slice::from_ref(&submitter), load).await?;
    let Some(submission) = list.first() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let urls = Urls::new(&state, submitter.account_id).await?;
    Ok(ok(&serialize::submitter_json(
        &urls,
        &data,
        submission,
        &submitter,
        SubmitterOpts {
            with_template: true,
            with_events: true,
            include_fields: params.str("include").is_some_and(|i| i.contains("fields")),
            ..SubmitterOpts::default()
        },
    )))
}

/// `SubmissionEvents.create_with_tracking_data`: the event with the request's IP, user
/// agent and session user, blank values dropped (`compact_blank`). API requests have no
/// session id.
pub async fn track_event(
    state: &AppState,
    submitter: &Submitter,
    event_type: &str,
    ip: Option<&str>,
    headers: &HeaderMap,
    user_id: Option<i64>,
) -> Result<(), AppError> {
    let mut data = serde_json::Map::new();
    if let Some(ip) = ip.filter(|ip| !ip.is_empty()) {
        data.insert("ip".into(), ip.into());
    }
    if let Some(ua) = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|ua| !ua.trim().is_empty())
    {
        data.insert("ua".into(), ua.into());
    }
    if let Some(user_id) = user_id {
        data.insert("uid".into(), user_id.into());
    }
    submissions::insert_event(
        &state.db,
        submitter,
        event_type,
        &JsonText(serde_json::Value::Object(data)),
    )
    .await?;
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct FormViewParams {
    submitter_slug: Option<String>,
}

/// `POST /api/submitter_form_views` (no authentication): marks the form as opened.
pub async fn form_view(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let slug = params
        .str("submitter_slug")
        .or_else(|| {
            serde_json::from_value::<FormViewParams>(params.0.clone())
                .ok()?
                .submitter_slug
        })
        .unwrap_or_default();
    let Some(submitter) = submissions::find_submitter_by_slug(&state.db, &slug).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    submissions::mark_opened(&state.db, submitter.id).await?;
    track_event(
        &state,
        &submitter,
        "view_form",
        ip.as_deref(),
        &headers,
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
    Ok(ok(&json!({})))
}
