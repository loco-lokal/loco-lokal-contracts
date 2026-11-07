//! Polymorphic records (`record_type`, `record_id`), as Active Storage attachments and
//! search entries reference them.

use sqlx::PgPool;

use crate::DbError;

/// Who owns a record, for authorization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    pub account_id: Option<i64>,
    pub user_id: Option<i64>,
}

/// The owner of an attachment record. `None` for a missing record or an unknown type.
pub async fn owner(
    db: &PgPool,
    record_type: &str,
    record_id: i64,
) -> Result<Option<Owner>, DbError> {
    let account = |account_id: Option<i64>| {
        account_id.map(|account_id| Owner {
            account_id: Some(account_id),
            user_id: None,
        })
    };
    Ok(match record_type {
        "Template" => account(
            sqlx::query_scalar!("SELECT account_id FROM templates WHERE id = $1", record_id)
                .fetch_optional(db)
                .await?,
        ),
        "Submission" => account(
            sqlx::query_scalar!(
                "SELECT account_id FROM submissions WHERE id = $1",
                record_id
            )
            .fetch_optional(db)
            .await?,
        ),
        "Submitter" => account(
            sqlx::query_scalar!("SELECT account_id FROM submitters WHERE id = $1", record_id)
                .fetch_optional(db)
                .await?,
        ),
        "Account" => account(
            sqlx::query_scalar!("SELECT id FROM accounts WHERE id = $1", record_id)
                .fetch_optional(db)
                .await?,
        ),
        "User" => sqlx::query!("SELECT id, account_id FROM users WHERE id = $1", record_id)
            .fetch_optional(db)
            .await?
            .map(|row| Owner {
                account_id: Some(row.account_id),
                user_id: Some(row.id),
            }),
        "DynamicDocument" => account(
            sqlx::query_scalar!(
                "SELECT t.account_id FROM dynamic_documents d
                 JOIN templates t ON t.id = d.template_id WHERE d.id = $1",
                record_id
            )
            .fetch_optional(db)
            .await?,
        ),
        "DynamicDocumentVersion" => account(
            sqlx::query_scalar!(
                "SELECT t.account_id FROM dynamic_document_versions v
                 JOIN dynamic_documents d ON d.id = v.dynamic_document_id
                 JOIN templates t ON t.id = d.template_id WHERE v.id = $1",
                record_id
            )
            .fetch_optional(db)
            .await?,
        ),
        _ => None,
    })
}
