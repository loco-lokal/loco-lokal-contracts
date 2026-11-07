//! `Api::FormEventsController` and `Api::SubmissionEventsController`: completion feeds
//! paginated by `completed_at` (Unix seconds in `after` / `before`).

use axum::{
    extract::{Path, State},
    response::Response,
};
use chrono::{DateTime, Duration, NaiveDateTime};
use loco_db::submissions::{self, TimePage};
use serde_json::{Value, json};

use super::{
    DEFAULT_LIMIT, MAX_LIMIT, Params, ok,
    serialize::{self, Load, SubmissionData, SubmissionOpts, Urls},
    submitters::load_for_submitters,
};
use crate::{auth::ApiAuth, errors::AppError, state::AppState};

fn time_page(params: &Params) -> TimePage {
    let at = |key: &str| {
        params
            .int(key)
            .and_then(|secs| DateTime::from_timestamp(secs, 0))
            .map(|t| t.naive_utc())
    };
    TimePage {
        limit: params.int("limit").unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT),
        after: at("after"),
        before: at("before").map(|t| t + Duration::seconds(1)),
    }
}

fn time_pagination(times: &[NaiveDateTime]) -> Value {
    json!({
        "count": times.len(),
        "next": times.last().map(|t| t.and_utc().timestamp()),
        "prev": times.first().map(|t| t.and_utc().timestamp()),
    })
}

/// `GET /api/events/form/:type`.
pub async fn form(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    Path(_event_type): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let list =
        submissions::completed_submitters(&state.db, auth.account.id, time_page(&params)).await?;
    let load = Load {
        events: true,
        documents: true,
        attachments: true,
    };
    let (submission_list, data) = load_for_submitters(&state, &list, load).await?;
    let urls = Urls::new(&state, auth.account.id).await?;

    let items: Vec<Value> = list
        .iter()
        .filter_map(|s| {
            let submission = submission_list.iter().find(|x| x.id == s.submission_id)?;
            Some(json!({
                "event_type": "form.completed",
                "timestamp": serialize::opt_time(s.completed_at),
                "data": serialize::webhook_submitter_json(&urls, &data, submission, s),
            }))
        })
        .collect();
    let times: Vec<NaiveDateTime> = list.iter().filter_map(|s| s.completed_at).collect();
    Ok(ok(
        &json!({ "data": items, "pagination": time_pagination(&times) }),
    ))
}

/// `GET /api/events/submission/:type`.
pub async fn submission(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    Path(_event_type): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let list =
        submissions::completed_submissions(&state.db, auth.account.id, time_page(&params)).await?;
    let data = SubmissionData::load(
        &state,
        &list,
        Load {
            events: true,
            documents: true,
            attachments: true,
        },
    )
    .await?;
    let urls = Urls::new(&state, auth.account.id).await?;

    let items: Vec<Value> = list
        .iter()
        .map(|s| {
            json!({
                "event_type": "submission.completed",
                "timestamp": serialize::opt_time(s.completed_at),
                "data": serialize::submission_json(&urls, &data, s, SubmissionOpts::default()),
            })
        })
        .collect();
    let times: Vec<NaiveDateTime> = list.iter().filter_map(|s| s.completed_at).collect();
    Ok(ok(
        &json!({ "data": items, "pagination": time_pagination(&times) }),
    ))
}
