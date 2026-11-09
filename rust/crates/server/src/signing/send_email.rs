//! `SendSubmissionEmailController` (`POST /send_submission_email`): the "send a copy to my
//! email" buttons of the completed pages.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use loco_db::{signing, submissions};
use loco_jobs::EnqueueOptions;
use serde_json::json;

use super::{Form, actions::redirect_to, emails::SendDocumentsCopy, pages::form_locale};
use crate::{
    api::Params, auth::MaybeAuth, errors::AppError, page::Page, session::Session, state::AppState,
};

/// `SEND_DURATION`: one copy per party within this time.
const SEND_DURATION: Duration = Duration::minutes(30);

fn not_found() -> AppError {
    AppError::Status(StatusCode::NOT_FOUND)
}

/// `POST /send_submission_email` (no CSRF check).
pub async fn create(
    State(state): State<AppState>,
    MaybeAuth(auth): MaybeAuth,
    session: Session,
    headers: HeaderMap,
    params: Params,
) -> Result<Response, AppError> {
    let email = params
        .get("email")
        .map(crate::api::create::values::ruby_to_s)
        .unwrap_or_default()
        .to_lowercase();
    let submitter = if let Some(slug) = params.str("template_slug") {
        let template = loco_db::templates::find_by_slug(&state.db, &slug)
            .await?
            .ok_or_else(not_found)?;
        signing::find_completed_start_form_submitter(&state.db, template.id, Some(&email), None)
            .await?
    } else if let Some(slug) = params.str("submission_slug") {
        let id = signing::submission_id_by_slug(&state.db, &slug)
            .await?
            .ok_or_else(not_found)?;
        // `submitters.order(:completed_at).find_by(email:)`: nulls last.
        let mut list: Vec<_> = submissions::submitters_for(&state.db, &[id])
            .await?
            .into_iter()
            .filter(|s| s.email.as_deref() == Some(email.as_str()))
            .collect();
        list.sort_by_key(|s| (s.completed_at.is_none(), s.completed_at));
        let Some(submitter) = list.into_iter().next() else {
            return Ok(redirect_to(
                &state,
                &headers,
                &format!("/e/{slug}/completed?status=error"),
            ));
        };
        Some(submitter)
    } else {
        let slug = params.str("submitter_slug").unwrap_or_default();
        Some(
            submissions::find_submitter_by_slug(&state.db, &slug)
                .await?
                .ok_or_else(not_found)?,
        )
    };

    if let Some(submitter) = submitter {
        let form = Form::for_submitter(&state, submitter).await?;
        let completed = form.submitter.completed_at.is_some()
            || (form.is_viewer(&form.submitter) && form.submission.completed_at.is_some());
        // `RateLimit.call("send-email-…")` only limits in multitenant mode.
        if completed && can_send(&state, &form).await? {
            loco_jobs::enqueue(
                &state.db,
                &SendDocumentsCopy {
                    submitter_id: form.submitter.id,
                    to: None,
                    sig: true,
                },
                EnqueueOptions::default(),
            )
            .await?;
        }
    }

    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if wants_json {
        return Ok(StatusCode::OK.into_response());
    }
    let locale = form_locale(
        &state,
        auth.as_ref(),
        params.str("lang").as_deref(),
        &headers,
    );
    Page::new(&state, &session, &locale)
        .auth(auth.as_ref())
        .with(json!({ "robots_noindex": true }))
        .render("send_submission_email/success.html")
}

/// `can_send?`: not for archived accounts, and once per `SEND_DURATION`.
async fn can_send(state: &AppState, form: &Form) -> Result<bool, AppError> {
    if form.account.archived_at.is_some() {
        return Ok(false);
    }
    let since = (Utc::now() - SEND_DURATION).naive_utc();
    let sent = match form.submitter.email.as_deref() {
        Some(email) => {
            signing::email_sent_since(
                &state.db,
                "submitter_documents_copy",
                email,
                "Submitter",
                form.submitter.id,
                since,
            )
            .await?
        }
        None => false,
    };
    Ok(!sent)
}
