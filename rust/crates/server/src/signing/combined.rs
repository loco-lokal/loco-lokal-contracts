//! `Submissions::GenerateCombinedAttachment` and `Submissions::EnsureCombinedGenerated`:
//! with the account's `combine_pdf_result_key`, the result documents and the audit trail
//! of a completed submission in one PDF, signed like the results and attached to the
//! submission as `combined_document`. Also `Submitters.build_combined_url`.

use std::time::Duration;

use chrono::Utc;
use loco_db::{signing, submissions::Submitter};
use loco_pdf::edit::Editor;
use loco_storage::{Attachment, Blob};
use serde_json::{Value, json};

use super::{
    Form,
    results::{SIGN_REASON, check_pdf_format, check_timestamp_server, load_signing_key, pdf_error},
};
use crate::{email_variables::Vars, errors::AppError, state::AppState};

const COMBINE_PDF_RESULT_KEY: &str = "combine_pdf_result_key";

/// `EnsureCombinedGenerated::CHECK_COMPLETE_TIMEOUT`.
const CHECK_COMPLETE_TIMEOUT: Duration = Duration::from_secs(90);

/// `account.account_configs.exists?(key: COMBINE_PDF_RESULT_KEY, value: true)`.
pub async fn enabled(state: &AppState, account_id: i64) -> Result<bool, AppError> {
    let rows = signing::account_configs(&state.db, account_id, &[COMBINE_PDF_RESULT_KEY]).await?;
    Ok(rows.iter().any(|(_, v)| *v == Value::Bool(true)))
}

/// `submission.combined_document_attachment`.
pub async fn combined_document(
    state: &AppState,
    submission_id: i64,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    Ok(Attachment::with_blobs_for_records(
        &state.db,
        "Submission",
        &[submission_id],
        "combined_document",
    )
    .await?
    .into_iter()
    .next())
}

/// `Submissions::EnsureCombinedGenerated.call(submitter)`: the combined document,
/// generated once per submitter (other workers wait for the one generating).
pub async fn ensure_combined_generated(
    state: &AppState,
    submitter: &Submitter,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    let submission = loco_db::submissions::find(&state.db, submitter.submission_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("submission {} missing", submitter.submission_id))?;
    if submitter.completed_at.is_none() || submission.completed_at.is_none() {
        return Err(anyhow::anyhow!("submitter {} has not completed yet", submitter.id).into());
    }
    let key = format!("combined_document:{}", submitter.id);
    let started = std::time::Instant::now();
    loop {
        let events = signing::lock_event_names(&state.db, &key).await?;
        if events.iter().any(|e| e == "complete") {
            return combined_document(state, submission.id).await;
        }
        if events.last().is_some_and(|e| e == "start" || e == "retry") {
            return wait_for_complete_or_fail(state, submission.id, &key).await;
        }
        let name = if events.is_empty() { "start" } else { "retry" };
        if !signing::create_lock_event(&state.db, &key, name).await? {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
                return Err(anyhow::anyhow!("timed out waiting for {key}").into());
            }
            continue;
        }
        return match Box::pin(generate(state, submitter)).await {
            Ok(attachment) => {
                signing::create_lock_event(&state.db, &key, "complete").await?;
                Ok(Some(attachment))
            }
            Err(err) => {
                tracing::error!(error = ?err, submitter_id = submitter.id, "generating the combined document failed");
                signing::create_lock_event(&state.db, &key, "fail").await?;
                Err(err)
            }
        };
    }
}

async fn wait_for_complete_or_fail(
    state: &AppState,
    submission_id: i64,
    key: &str,
) -> Result<Option<(Attachment, Blob)>, AppError> {
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = signing::lock_event_names(&state.db, key).await?;
        if events
            .last()
            .is_some_and(|e| e == "complete" || e == "fail")
        {
            return combined_document(state, submission_id).await;
        }
        if started.elapsed() > CHECK_COMPLETE_TIMEOUT {
            return Err(anyhow::anyhow!("timed out waiting for {key}").into());
        }
    }
}

/// `submission.combined_document_attachment || EnsureCombinedGenerated.call(submitter)`.
pub async fn find_or_generate(
    state: &AppState,
    submitter: &Submitter,
) -> Result<(Attachment, Blob), AppError> {
    if let Some(found) = combined_document(state, submitter.submission_id).await? {
        return Ok(found);
    }
    ensure_combined_generated(state, submitter)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("combined document of submitter {} missing", submitter.id).into()
        })
}

/// `Submitters.select_attachments_for_download(submitter)`: the combined document for
/// completed submissions without verification fields when the account combines results,
/// else the result documents.
pub(super) async fn select_attachments_for_download(
    state: &AppState,
    form: &Form,
) -> Result<Vec<(Attachment, Blob)>, AppError> {
    if form.submission.completed_at.is_some()
        && !form
            .fields()
            .iter()
            .any(|f| f.get("type").and_then(Value::as_str) == Some("verification"))
        && enabled(state, form.account.id).await?
    {
        return Ok(vec![find_or_generate(state, &form.submitter).await?]);
    }
    super::audit::select_attachments(state, form).await
}

/// `GenerateCombinedAttachment.call(submitter)`: the documents as `generate_pdfs` fills
/// them for the submitter, in schema order, then the audit trail.
async fn generate(state: &AppState, submitter: &Submitter) -> Result<(Attachment, Blob), AppError> {
    check_pdf_format()?;
    let form = Form::for_submitter(state, submitter.clone()).await?;
    let mut pdfs = super::results::generate_pdfs(state, &form).await?;
    let audit_trail = Box::pin(super::audit::build_audit_trail(state, form.submission.id)).await?;

    let mut result = Editor::blank().map_err(pdf_error)?;
    for item in form.schema() {
        let Some(uuid) = item.get("attachment_uuid").and_then(Value::as_str) else {
            continue;
        };
        let Some(index) = pdfs.iter().position(|p| p.uuid == uuid) else {
            continue;
        };
        let pdf = pdfs.remove(index);
        let document = pdf.editor.into_document().map_err(pdf_error)?;
        result.import_pages(&document).map_err(pdf_error)?;
    }
    let document = audit_trail.into_document().map_err(pdf_error)?;
    result.import_pages(&document).map_err(pdf_error)?;

    let key = load_signing_key(state, form.account.id).await?;
    if key.is_some() {
        check_timestamp_server(state, form.account.id).await?;
    }

    let now = Utc::now();
    result.set_info_text("Creator", "DocuSeal (https://www.docuseal.com)");
    let date = now.format("D:%Y%m%d%H%M%S").to_string();
    let bytes = match &key {
        // `single_sign_reason`: `SIGN_REASON` has no `%{name}`.
        Some(key) => result
            .save_signed(
                &date,
                &now.format("%Y%m%d%H%M%S").to_string(),
                SIGN_REASON,
                key,
            )
            .map_err(pdf_error)?,
        None => result.save(&date).map_err(pdf_error)?,
    };

    let name = form
        .submission
        .name
        .clone()
        .or_else(|| form.template.as_ref().map(|t| t.name.clone()))
        .unwrap_or_default();
    let filename = format!("{name}.pdf").replace('/', "-");
    let blob = state
        .storage
        .create_and_upload_exact(
            &state.db,
            bytes.into(),
            &filename,
            Some("application/pdf"),
            json!({ "identified": true, "analyzed": true }),
        )
        .await?;
    let attachment = Attachment::create(
        &state.db,
        "Submission",
        form.submission.id,
        "combined_document",
        &blob,
    )
    .await?;
    Ok((attachment, blob))
}

/// `Submitters.build_combined_url(submitter)`: a link to the combined document, or
/// `None` before the submission completes or for a party that did not complete last.
pub async fn build_combined_url(state: &AppState, form: &Form) -> Result<Option<String>, AppError> {
    if form.submission.completed_at.is_none() {
        return Ok(None);
    }
    let last = form
        .submitters
        .iter()
        .filter(|s| s.completed_at.is_some())
        .max_by_key(|s| s.completed_at);
    if last.map(|s| s.id) != Some(form.submitter.id) {
        return Ok(None);
    }
    let (_, mut blob) = find_or_generate(state, &form.submitter).await?;
    let filename_format = super::emails::filename_format(state, form.submitter.account_id).await?;
    let mut vars = Vars::load(state, form.submitter.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("submitter without submission"))?;
    let filename =
        super::emails::document_filename(state, &mut vars, &blob, filename_format.as_deref())
            .await?;
    Ok(Some(
        super::downloads::proxy_path(state, &mut blob, Some(&filename)).await?,
    ))
}
