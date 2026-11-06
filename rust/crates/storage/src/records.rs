//! Rows of `active_storage_blobs` and `active_storage_attachments`.

use chrono::NaiveDateTime;
use sqlx::PgPool;

#[derive(Clone, Debug)]
pub struct Blob {
    pub id: i64,
    pub key: String,
    pub filename: String,
    pub content_type: Option<String>,
    /// JSON text, e.g. `{"identified":true,"analyzed":true,"width":…}`.
    pub metadata: Option<String>,
    pub service_name: String,
    pub byte_size: i64,
    pub checksum: Option<String>,
    pub uuid: Option<String>,
    pub created_at: NaiveDateTime,
}

pub struct NewBlob<'a> {
    pub key: &'a str,
    pub filename: &'a str,
    pub content_type: Option<&'a str>,
    pub metadata: &'a serde_json::Value,
    pub service_name: &'a str,
    pub byte_size: i64,
    pub checksum: &'a str,
}

impl Blob {
    pub(crate) async fn insert(db: &PgPool, new: NewBlob<'_>) -> Result<Self, sqlx::Error> {
        sqlx::query_as!(
            Blob,
            r#"
            INSERT INTO active_storage_blobs
              (key, filename, content_type, metadata, service_name, byte_size, checksum, uuid, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, (now() AT TIME ZONE 'utc'))
            RETURNING id, key, filename, content_type, metadata, service_name, byte_size,
                      checksum, uuid, created_at
            "#,
            new.key,
            new.filename,
            new.content_type,
            new.metadata.to_string(),
            new.service_name,
            new.byte_size,
            new.checksum,
            uuid::Uuid::new_v4().to_string(),
        )
        .fetch_one(db)
        .await
    }

    pub async fn find(db: &PgPool, id: i64) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Blob,
            r#"
            SELECT id, key, filename, content_type, metadata, service_name, byte_size,
                   checksum, uuid, created_at
            FROM active_storage_blobs WHERE id = $1
            "#,
            id
        )
        .fetch_optional(db)
        .await
    }

    pub async fn find_by_uuid(db: &PgPool, uuid: &str) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Blob,
            r#"
            SELECT id, key, filename, content_type, metadata, service_name, byte_size,
                   checksum, uuid, created_at
            FROM active_storage_blobs WHERE uuid = $1
            "#,
            uuid
        )
        .fetch_optional(db)
        .await
    }

    #[must_use]
    pub fn metadata_json(&self) -> serde_json::Value {
        self.metadata
            .as_deref()
            .and_then(|m| serde_json::from_str(m).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: i64,
    pub name: String,
    pub record_type: String,
    pub record_id: i64,
    pub blob_id: i64,
    pub uuid: String,
    pub created_at: NaiveDateTime,
}

impl Attachment {
    /// Attaches `blob` to a record, e.g. `("Template", 12, "documents")`.
    pub async fn create(
        db: &PgPool,
        record_type: &str,
        record_id: i64,
        name: &str,
        blob: &Blob,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            INSERT INTO active_storage_attachments (name, record_type, record_id, blob_id, uuid, created_at)
            VALUES ($1, $2, $3, $4, $5, (now() AT TIME ZONE 'utc'))
            RETURNING id, name, record_type, record_id, blob_id, uuid, created_at
            "#,
            name,
            record_type,
            record_id,
            blob.id,
            uuid::Uuid::new_v4().to_string(),
        )
        .fetch_one(db)
        .await
    }

    /// Attachments of a record under `name`, in insertion order (`has_many_attached`).
    pub async fn for_record(
        db: &PgPool,
        record_type: &str,
        record_id: i64,
        name: &str,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            SELECT id, name, record_type, record_id, blob_id, uuid, created_at
            FROM active_storage_attachments
            WHERE record_type = $1 AND record_id = $2 AND name = $3
            ORDER BY id
            "#,
            record_type,
            record_id,
            name
        )
        .fetch_all(db)
        .await
    }

    pub async fn find_by_uuid(db: &PgPool, uuid: &str) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            SELECT id, name, record_type, record_id, blob_id, uuid, created_at
            FROM active_storage_attachments WHERE uuid = $1
            "#,
            uuid
        )
        .fetch_optional(db)
        .await
    }

    pub async fn blob(&self, db: &PgPool) -> Result<Blob, sqlx::Error> {
        Blob::find(db, self.blob_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }
}
