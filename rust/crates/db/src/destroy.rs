//! Permanent deletes with Rails' `dependent: :destroy` cascades. Attachments are
//! removed with their preview images; the returned blob ids are for a purge job, as
//! `purge_later` (the blob is deleted once nothing else is attached to it).

use sqlx::{Postgres, Transaction};

use crate::DbError;

/// Deletes the attachments of `(record_type, ids)` and the preview images attached to
/// them. Returns the blob ids they used.
async fn delete_attachments(
    tx: &mut Transaction<'_, Postgres>,
    record_type: &str,
    record_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    if record_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar!(
        r#"WITH parents AS (
             DELETE FROM active_storage_attachments
             WHERE record_type = $1 AND record_id = ANY($2)
             RETURNING id, blob_id
           ), previews AS (
             DELETE FROM active_storage_attachments
             WHERE record_type = 'ActiveStorage::Attachment'
               AND record_id IN (SELECT id FROM parents)
             RETURNING blob_id
           )
           SELECT blob_id AS "blob_id!" FROM parents
           UNION SELECT blob_id FROM previews"#,
        record_type,
        record_ids,
    )
    .fetch_all(&mut **tx)
    .await?)
}

async fn delete_search_entries(
    tx: &mut Transaction<'_, Postgres>,
    record_type: &str,
    record_ids: &[i64],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM search_entries WHERE record_type = $1 AND record_id = ANY($2)",
        record_type,
        record_ids
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// `submission.destroy!`: submitters (with their versions, events, generation events,
/// email events and attachments), events and attachments.
pub async fn submissions(
    tx: &mut Transaction<'_, Postgres>,
    submission_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let submitter_ids = sqlx::query_scalar!(
        "SELECT id FROM submitters WHERE submission_id = ANY($1)",
        submission_ids
    )
    .fetch_all(&mut **tx)
    .await?;

    let mut blobs = delete_attachments(tx, "Submitter", &submitter_ids).await?;
    sqlx::query!(
        "DELETE FROM submitter_versions WHERE submitter_id = ANY($1)",
        &submitter_ids
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM document_generation_events WHERE submitter_id = ANY($1)",
        &submitter_ids
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM email_events WHERE emailable_type = 'Submitter' AND emailable_id = ANY($1)",
        &submitter_ids
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM submission_events WHERE submission_id = ANY($1)",
        submission_ids
    )
    .execute(&mut **tx)
    .await?;
    delete_search_entries(tx, "Submitter", &submitter_ids).await?;
    sqlx::query!("DELETE FROM submitters WHERE id = ANY($1)", &submitter_ids)
        .execute(&mut **tx)
        .await?;

    blobs.extend(delete_attachments(tx, "Submission", submission_ids).await?);
    delete_search_entries(tx, "Submission", submission_ids).await?;
    sqlx::query!("DELETE FROM submissions WHERE id = ANY($1)", submission_ids)
        .execute(&mut **tx)
        .await?;
    Ok(blobs)
}

/// `template.destroy!`: submissions, sharings, accesses, versions, dynamic documents
/// and documents.
pub async fn template(
    tx: &mut Transaction<'_, Postgres>,
    template_id: i64,
) -> Result<Vec<i64>, DbError> {
    let submission_ids = sqlx::query_scalar!(
        "SELECT id FROM submissions WHERE template_id = $1",
        template_id
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut blobs = submissions(tx, &submission_ids).await?;

    for query in [
        "DELETE FROM template_sharings WHERE template_id = $1",
        "DELETE FROM template_accesses WHERE template_id = $1",
        "DELETE FROM template_versions WHERE template_id = $1",
    ] {
        sqlx::query(query)
            .bind(template_id)
            .execute(&mut **tx)
            .await?;
    }

    let dynamic_ids = sqlx::query_scalar!(
        "SELECT id FROM dynamic_documents WHERE template_id = $1",
        template_id
    )
    .fetch_all(&mut **tx)
    .await?;
    let version_ids = sqlx::query_scalar!(
        "SELECT id FROM dynamic_document_versions WHERE dynamic_document_id = ANY($1)",
        &dynamic_ids
    )
    .fetch_all(&mut **tx)
    .await?;
    blobs.extend(delete_attachments(tx, "DynamicDocumentVersion", &version_ids).await?);
    blobs.extend(delete_attachments(tx, "DynamicDocument", &dynamic_ids).await?);
    sqlx::query!(
        "DELETE FROM dynamic_document_versions WHERE id = ANY($1)",
        &version_ids
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM dynamic_documents WHERE id = ANY($1)",
        &dynamic_ids
    )
    .execute(&mut **tx)
    .await?;

    blobs.extend(delete_attachments(tx, "Template", &[template_id]).await?);
    delete_search_entries(tx, "Template", &[template_id]).await?;
    sqlx::query!("DELETE FROM templates WHERE id = $1", template_id)
        .execute(&mut **tx)
        .await?;

    blobs.sort_unstable();
    blobs.dedup();
    Ok(blobs)
}
