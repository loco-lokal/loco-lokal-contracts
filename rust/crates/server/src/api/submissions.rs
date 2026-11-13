//! `Api::SubmissionsController` and `Api::SubmissionDocumentsController`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{DateTime, Utc};
use loco_db::{
    search,
    submissions::{self, Submission, SubmissionFilter, SubmissionUpdate, Submitter},
    templates, users,
};
use serde_json::{Value, json};

use super::{
    Params, access_denied, like_pattern, ok, page, pagination,
    serialize::{self, Load, SubmissionData, SubmissionOpts, Urls, filename_base},
    time::{account_zone, end_of_day, parse_in_zone},
};
use crate::{
    auth::{ApiAuth, Auth},
    errors::AppError,
    purge,
    state::AppState,
    webhooks::{self, EventRecord},
};

pub use super::create::{create, create_emails, create_for_template, create_init};

/// The submission, or the response for a missing or forbidden one.
async fn authorized_submission(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    id: &str,
) -> Result<Result<Submission, Response>, AppError> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    let Some(submission) = submissions::find(&state.db, id).await? else {
        return Err(AppError::Status(StatusCode::NOT_FOUND));
    };
    if auth.ability().can_account_record(submission.account_id) {
        Ok(Ok(submission))
    } else {
        Ok(Err(access_denied(
            state,
            auth,
            headers,
            "Submission",
            submission.id,
            Some(submission.account_id),
        )
        .await?))
    }
}

/// `params[:include]` mentions `name`.
fn includes(params: &Params, name: &str) -> bool {
    params.str("include").is_some_and(|i| i.contains(name))
}

/// `maybe_build_combined_url` with `include=combined_document_url`: generates the
/// combined document of completed submissions that have none yet.
async fn maybe_generate_combined(
    state: &AppState,
    params: &Params,
    submissions: &[Submission],
) -> Result<(), AppError> {
    if !includes(params, "combined_document_url") {
        return Ok(());
    }
    for submission in submissions.iter().filter(|s| s.completed_at.is_some()) {
        if crate::signing::combined::combined_document(state, submission.id)
            .await?
            .is_some()
        {
            continue;
        }
        let submitters = submissions::submitters_for(&state.db, &[submission.id]).await?;
        // `submitters.select(&:completed_at?).max_by(&:completed_at)`
        let mut last: Option<&Submitter> = None;
        for s in &submitters {
            if let Some(at) = s.completed_at
                && last.is_none_or(|l| l.completed_at.is_some_and(|l| at > l))
            {
                last = Some(s);
            }
        }
        if let Some(last) = last {
            crate::signing::combined::ensure_combined_generated(state, last).await?;
        }
    }
    Ok(())
}

fn parse_date(params: &Params, key: &str, auth: &Auth) -> Option<DateTime<Utc>> {
    params
        .str(key)
        .and_then(|v| parse_in_zone(&v, account_zone(&auth.account.timezone)))
}

/// `GET /api/submissions`.
pub async fn index(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    params: Params,
) -> Result<Response, AppError> {
    list(&state, &auth, &params, None).await
}

/// `GET /api/templates/:template_id/submissions`.
pub async fn index_for_template(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    Path(template_id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    list(&state, &auth, &params, Some(template_id)).await
}

async fn list(
    state: &AppState,
    auth: &Auth,
    params: &Params,
    template_id: Option<String>,
) -> Result<Response, AppError> {
    let account_id = auth.account.id;
    let tz = account_zone(&auth.account.timezone);
    let empty = || Ok(ok(&json!({ "data": [], "pagination": pagination(&[]) })));

    // `template_folder` (accessible folders) and `Submissions::Filter`'s `folder`
    // (with subfolders) both narrow by the template's folder.
    let mut folder_ids: Option<Vec<i64>> = None;
    if let Some(name) = params.str("template_folder") {
        folder_ids =
            Some(templates::filter_folders_by_full_name(&state.db, account_id, &name).await?);
    }
    if let Some(name) = params.str("folder") {
        let mut ids = templates::filter_folders_by_full_name(&state.db, account_id, &name).await?;
        ids.extend(templates::subfolder_ids(&state.db, &ids).await?);
        folder_ids = Some(match folder_ids {
            Some(existing) => existing.into_iter().filter(|id| ids.contains(id)).collect(),
            None => ids,
        });
    }

    let created_by_user_id = match params.str("author") {
        Some(email) => Some(
            users::find_id_in_account_by_email(&state.db, account_id, &email)
                .await?
                .unwrap_or(-1),
        ),
        None => None,
    };

    let template_id = match template_id.or_else(|| params.str("template_id")) {
        Some(id) => match id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => return empty(),
        },
        None => None,
    };

    let (search, search_ids) = match params.str("q") {
        None => (None, None),
        Some(q) if search::enabled(&state.db).await? => (
            None,
            Some(search::matching_submission_ids(&state.db, auth.account.id, &q).await?),
        ),
        Some(q) => (Some(like_pattern(&q)), None),
    };
    let slug = params.str("slug");
    let status = params.str("status");
    let filter = SubmissionFilter {
        account_id,
        template_id,
        slug: slug.as_deref(),
        search_like: search.as_deref(),
        ids: search_ids.as_deref(),
        folder_ids: folder_ids.as_deref(),
        archived: params.has("archived").then(|| params.is_true("archived")),
        created_by_user_id,
        status: status.as_deref(),
        created_at_from: parse_date(params, "created_at_from", auth).map(|t| t.naive_utc()),
        created_at_to: parse_date(params, "created_at_to", auth)
            .map(|t| end_of_day(t, tz).naive_utc()),
        completed_at_from: parse_date(params, "completed_at_from", auth).map(|t| t.naive_utc()),
        completed_at_to: parse_date(params, "completed_at_to", auth)
            .map(|t| end_of_day(t, tz).naive_utc()),
    };
    let list = submissions::list(&state.db, &filter, page(params)).await?;
    maybe_generate_combined(state, params, &list).await?;

    let urls = Urls::new(state, account_id).await?;
    let data = SubmissionData::load(state, &list, Load::default()).await?;
    let opts = SubmissionOpts {
        with_events: false,
        with_documents: false,
        with_values: false,
        include_fields: includes(params, "fields"),
    };
    let ids: Vec<i64> = list.iter().map(|s| s.id).collect();
    Ok(ok(&json!({
        "data": list.iter().map(|s| serialize::submission_json(&urls, &data, s, opts)).collect::<Vec<_>>(),
        "pagination": pagination(&ids),
    })))
}

/// `GET /api/submissions/:id`.
pub async fn show(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submission = match authorized_submission(&state, &auth, &headers, &id).await? {
        Ok(submission) => submission,
        Err(response) => return Ok(response),
    };
    // Completed documents and audit trails missing here are generated by
    // `Submissions::EnsureResultGenerated` / `EnsureAuditGenerated` in Rails; the Rust
    // generators arrive with the signing flow (Phase 4).
    maybe_generate_combined(&state, &params, std::slice::from_ref(&submission)).await?;
    let urls = Urls::new(&state, submission.account_id).await?;
    let data = SubmissionData::load(
        &state,
        std::slice::from_ref(&submission),
        Load {
            events: true,
            documents: true,
            attachments: true,
        },
    )
    .await?;
    Ok(ok(&serialize::submission_json(
        &urls,
        &data,
        &submission,
        SubmissionOpts {
            include_fields: includes(&params, "fields"),
            ..SubmissionOpts::default()
        },
    )))
}

/// `value.in?(Submitters::TRUE_VALUES)` (`'1'`, `'true'`, `true`).
#[must_use]
pub fn truthy(value: &Value) -> bool {
    matches!(value, Value::Bool(true)) || matches!(value.as_str(), Some("1" | "true"))
}

/// `PUT /api/submissions/:id`.
pub async fn update(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submission = match authorized_submission(&state, &auth, &headers, &id).await? {
        Ok(submission) => submission,
        Err(response) => return Ok(response),
    };
    let attrs = params.nested_or_root("submission");
    let attrs = super::permit::permit(
        &attrs,
        &[
            ("name", super::permit::Rule::Scalar),
            ("expire_at", super::permit::Rule::Scalar),
            ("archived", super::permit::Rule::Scalar),
            ("archived_at", super::permit::Rule::Scalar),
        ],
    );

    let mut changes = SubmissionUpdate {
        name: submission.name.clone(),
        expire_at: submission.expire_at,
        archived_at: submission.archived_at,
    };
    let archived = attrs.get("archived").or_else(|| attrs.get("archived_at"));
    if let Some(archived) = archived
        .filter(|v| matches!(v, Value::Bool(_)) || matches!(v.as_str(), Some("true" | "false")))
    {
        // `can :destroy` on the submission: the same account, already checked.
        changes.archived_at = truthy(archived).then(|| Utc::now().naive_utc());
    }
    if let Some(name) = attrs.get("name") {
        changes.name = super::create::strip(super::templates::cast_string(name));
    }
    if let Some(expire_at) = attrs.get("expire_at") {
        // Like Active Record's datetime cast, an unparseable value is nil.
        changes.expire_at = super::params::string(expire_at)
            .and_then(|value| parse_in_zone(&value, chrono_tz::Tz::UTC))
            .map(|t| t.naive_utc());
    }

    let updated = submissions::update(&state.db, submission.id, &changes).await?;
    let record = [EventRecord {
        id: updated.id,
        account_id: updated.account_id,
    }];
    if updated.name != submission.name {
        crate::search::reindex_later(&state, "Submission", &[updated.id]).await;
    }
    if updated.archived_at.is_some() && updated.archived_at != submission.archived_at {
        webhooks::enqueue(&state, "submission.archived", &record).await;
    }
    if updated.expire_at.is_some() && updated.expire_at != submission.expire_at {
        // `ProcessSubmissionExpiredJob` comes with the expiration jobs (Phase 7).
        tracing::debug!(submission_id = updated.id, "expire_at changed");
    }

    maybe_generate_combined(&state, &params, std::slice::from_ref(&updated)).await?;
    let urls = Urls::new(&state, updated.account_id).await?;
    let data = SubmissionData::load(
        &state,
        std::slice::from_ref(&updated),
        Load {
            events: false,
            documents: true,
            attachments: true,
        },
    )
    .await?;
    Ok(ok(&serialize::submission_json(
        &urls,
        &data,
        &updated,
        SubmissionOpts {
            with_events: false,
            include_fields: includes(&params, "fields"),
            ..SubmissionOpts::default()
        },
    )))
}

/// `DELETE /api/submissions/:id`: archives, or deletes with `permanently=true`.
pub async fn destroy(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submission = match authorized_submission(&state, &auth, &headers, &id).await? {
        Ok(submission) => submission,
        Err(response) => return Ok(response),
    };
    let archived_at = if params.is_true("permanently") {
        let mut tx = state.db.begin().await?;
        let blobs = loco_db::destroy::submissions(&mut tx, &[submission.id]).await?;
        tx.commit().await?;
        purge::purge_later(&state, blobs).await?;
        submission.archived_at
    } else {
        let archived_at = submissions::set_archived(&state.db, submission.id, true).await?;
        webhooks::enqueue(
            &state,
            "submission.archived",
            &[EventRecord {
                id: submission.id,
                account_id: submission.account_id,
            }],
        )
        .await;
        archived_at
    };
    Ok(ok(&json!({
        "id": submission.id,
        "archived_at": serialize::opt_time(archived_at),
    })))
}

/// `GET /api/submissions/:id/documents`: the completed documents, or previews of a
/// pending submission. Generating missing ones (results, merged and preview PDFs) is
/// part of the PDF work in Phase 4; until then only stored documents are listed.
pub async fn documents(
    State(state): State<AppState>,
    ApiAuth(auth): ApiAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    params: Params,
) -> Result<Response, AppError> {
    let submission = match authorized_submission(&state, &auth, &headers, &id).await? {
        Ok(submission) => submission,
        Err(response) => return Ok(response),
    };
    let merge = params.str("merge").as_deref() == Some("true");
    let data = SubmissionData::load(
        &state,
        std::slice::from_ref(&submission),
        Load {
            events: false,
            documents: true,
            attachments: false,
        },
    )
    .await?;
    let urls = Urls::new(&state, submission.account_id).await?;

    let blobs = if submission.completed_at.is_some() {
        let last = data
            .submitters_of(submission.id)
            .iter()
            .filter(|s| s.completed_at.is_some())
            .max_by_key(|s| s.completed_at);
        if merge {
            stored(&state, "Submission", submission.id, "merged_document").await?
        } else {
            match last {
                Some(last) => stored(&state, "Submitter", last.id, "documents").await?,
                None => Vec::new(),
            }
        }
    } else if merge {
        stored(
            &state,
            "Submission",
            submission.id,
            "preview_merged_document",
        )
        .await?
    } else {
        stored(&state, "Submission", submission.id, "preview_documents").await?
    };

    Ok(ok(&json!({
        "id": submission.id,
        "documents": blobs.iter().map(|blob| json!({
            "name": filename_base(&blob.filename),
            "url": urls.blob(blob),
        })).collect::<Vec<_>>(),
    })))
}

async fn stored(
    state: &AppState,
    record_type: &str,
    record_id: i64,
    name: &str,
) -> Result<Vec<loco_storage::Blob>, AppError> {
    let mut blobs = Vec::new();
    for (_, mut blob) in
        loco_storage::Attachment::with_blobs_for_records(&state.db, record_type, &[record_id], name)
            .await?
    {
        blob.ensure_uuid(&state.db).await?;
        blobs.push(blob);
    }
    Ok(blobs)
}
