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

    /// `ActiveStorage::Blob.find_by(checksum:)` (the first by id).
    pub async fn find_by_checksum(
        db: &PgPool,
        checksum: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Blob,
            r#"
            SELECT id, key, filename, content_type, metadata, service_name, byte_size, checksum,
                   uuid, created_at
            FROM active_storage_blobs WHERE checksum = $1 ORDER BY id LIMIT 1
            "#,
            checksum
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

    /// `ActiveStorage::Blob#uuid`: blobs created before the column existed get one on
    /// first use.
    pub async fn ensure_uuid(&mut self, db: &PgPool) -> Result<String, sqlx::Error> {
        if let Some(uuid) = &self.uuid {
            return Ok(uuid.clone());
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        sqlx::query!(
            "UPDATE active_storage_blobs SET uuid = $2 WHERE id = $1",
            self.id,
            uuid
        )
        .execute(db)
        .await?;
        self.uuid = Some(uuid.clone());
        Ok(uuid)
    }

    /// Deletes the blob row if no attachment uses it any more (`purge`), returning it so
    /// the caller can delete the file.
    pub async fn delete_if_unattached(db: &PgPool, id: i64) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query!(
            "DELETE FROM active_storage_variant_records WHERE blob_id = $1
               AND NOT EXISTS (SELECT 1 FROM active_storage_attachments WHERE blob_id = $1)",
            id
        )
        .execute(db)
        .await?;
        sqlx::query_as!(
            Blob,
            r#"
            DELETE FROM active_storage_blobs b WHERE b.id = $1
              AND NOT EXISTS (SELECT 1 FROM active_storage_attachments a WHERE a.blob_id = b.id)
            RETURNING id, key, filename, content_type, metadata, service_name, byte_size,
                      checksum, uuid, created_at
            "#,
            id
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

    /// Attaches an existing blob under a chosen uuid (`documents_attachments.new(uuid:,
    /// blob_id:)`), as template clones do.
    pub async fn create_for_blob(
        db: impl sqlx::PgExecutor<'_>,
        record_type: &str,
        record_id: i64,
        name: &str,
        blob_id: i64,
        uuid: &str,
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
            blob_id,
            uuid,
        )
        .fetch_one(db)
        .await
    }

    pub async fn find(db: &PgPool, id: i64) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            SELECT id, name, record_type, record_id, blob_id, uuid, created_at
            FROM active_storage_attachments WHERE id = $1
            "#,
            id
        )
        .fetch_optional(db)
        .await
    }

    /// `blob.attachments.take`.
    pub async fn first_for_blob(db: &PgPool, blob_id: i64) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            Attachment,
            r#"
            SELECT id, name, record_type, record_id, blob_id, uuid, created_at
            FROM active_storage_attachments WHERE blob_id = $1 ORDER BY id LIMIT 1
            "#,
            blob_id
        )
        .fetch_optional(db)
        .await
    }

    /// Attachments of several records under `name`, with their blobs, in id order.
    pub async fn with_blobs_for_records(
        db: &PgPool,
        record_type: &str,
        record_ids: &[i64],
        name: &str,
    ) -> Result<Vec<(Self, Blob)>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"
            SELECT a.id, a.name, a.record_type, a.record_id, a.blob_id, a.uuid, a.created_at,
                   b.key, b.filename, b.content_type, b.metadata, b.service_name, b.byte_size,
                   b.checksum, b.uuid AS blob_uuid, b.created_at AS blob_created_at
            FROM active_storage_attachments a
            JOIN active_storage_blobs b ON b.id = a.blob_id
            WHERE a.record_type = $1 AND a.record_id = ANY($2) AND a.name = $3
            ORDER BY a.id
            "#,
            record_type,
            record_ids,
            name
        )
        .fetch_all(db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    Attachment {
                        id: r.id,
                        name: r.name,
                        record_type: r.record_type,
                        record_id: r.record_id,
                        blob_id: r.blob_id,
                        uuid: r.uuid,
                        created_at: r.created_at,
                    },
                    Blob {
                        id: r.blob_id,
                        key: r.key,
                        filename: r.filename,
                        content_type: r.content_type,
                        metadata: r.metadata,
                        service_name: r.service_name,
                        byte_size: r.byte_size,
                        checksum: r.checksum,
                        uuid: r.blob_uuid,
                        created_at: r.blob_created_at,
                    },
                )
            })
            .collect())
    }

    pub async fn blob(&self, db: &PgPool) -> Result<Blob, sqlx::Error> {
        Blob::find(db, self.blob_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }
}
