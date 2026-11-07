//! Active Storage–compatible blob storage.
//!
//! Blobs and attachments use the existing `active_storage_blobs` and
//! `active_storage_attachments` tables, and files sit at the same object keys, so files
//! uploaded by Rails and by Rust are interchangeable. On disk, Rails stores a blob at
//! `root/<key[0..2]>/<key[2..4]>/<key>`.

pub mod config;
mod records;

use std::{collections::HashMap, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use md5::{Digest, Md5};
use object_store::{
    ObjectStore, ObjectStoreExt as _, PutPayload, aws::AmazonS3Builder,
    azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder, local::LocalFileSystem,
    path::Path as ObjectPath,
};
use rand::RngExt as _;

pub use config::{ServiceConfig, StorageConfig};
pub use records::{Attachment, Blob, NewBlob};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage service {0:?} is not configured")]
    UnknownService(String),
    #[error("invalid storage configuration for {service}: {source}")]
    Config {
        service: String,
        source: object_store::Error,
    },
    #[error(transparent)]
    Store(#[from] object_store::Error),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("checksum mismatch for blob {0}")]
    Integrity(String),
}

impl StorageError {
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Store(object_store::Error::NotFound { .. }))
    }
}

#[derive(Clone)]
pub struct Storage {
    default_service: String,
    services: Arc<HashMap<String, Service>>,
}

#[derive(Clone)]
struct Service {
    store: Arc<dyn ObjectStore>,
    sharded_disk_layout: bool,
}

impl Storage {
    pub fn new(config: &StorageConfig) -> Result<Self, StorageError> {
        let mut services = HashMap::new();

        for (name, service) in &config.services {
            let config_error = |source| StorageError::Config {
                service: name.clone(),
                source,
            };

            let (store, sharded_disk_layout): (Arc<dyn ObjectStore>, bool) = match service {
                ServiceConfig::Disk { root } => {
                    std::fs::create_dir_all(root).map_err(|e| {
                        config_error(object_store::Error::Generic {
                            store: "LocalFileSystem",
                            source: Box::new(e),
                        })
                    })?;
                    (
                        Arc::new(LocalFileSystem::new_with_prefix(root).map_err(config_error)?),
                        true,
                    )
                }
                ServiceConfig::S3 {
                    access_key_id,
                    secret_access_key,
                    region,
                    bucket,
                    endpoint,
                } => {
                    let mut builder = AmazonS3Builder::from_env()
                        .with_region(region)
                        .with_bucket_name(bucket);
                    if let (Some(id), Some(secret)) = (access_key_id, secret_access_key) {
                        builder = builder
                            .with_access_key_id(id)
                            .with_secret_access_key(secret);
                    }
                    if let Some(endpoint) = endpoint {
                        // Rails sets force_path_style when an endpoint is given.
                        builder = builder
                            .with_endpoint(endpoint)
                            .with_virtual_hosted_style_request(false);
                    }
                    (Arc::new(builder.build().map_err(config_error)?), false)
                }
                ServiceConfig::Gcs {
                    credentials,
                    bucket,
                } => {
                    let mut builder =
                        GoogleCloudStorageBuilder::from_env().with_bucket_name(bucket);
                    if credentials.trim() != "{}" {
                        builder = builder.with_service_account_key(credentials);
                    }
                    (Arc::new(builder.build().map_err(config_error)?), false)
                }
                ServiceConfig::Azure {
                    account,
                    access_key,
                    container,
                } => {
                    let mut builder = MicrosoftAzureBuilder::from_env()
                        .with_account(account)
                        .with_container_name(container);
                    if let Some(key) = access_key {
                        builder = builder.with_access_key(key);
                    }
                    (Arc::new(builder.build().map_err(config_error)?), false)
                }
            };

            services.insert(
                name.clone(),
                Service {
                    store,
                    sharded_disk_layout,
                },
            );
        }

        if !services.contains_key(&config.default_service) {
            return Err(StorageError::UnknownService(config.default_service.clone()));
        }

        Ok(Self {
            default_service: config.default_service.clone(),
            services: Arc::new(services),
        })
    }

    #[must_use]
    pub fn default_service(&self) -> &str {
        &self.default_service
    }

    /// Uploads bytes to the default service and records the blob, marked `identified`
    /// as `ActiveStorage::Blob.create_and_upload!` does.
    pub async fn create_and_upload(
        &self,
        db: &sqlx::PgPool,
        data: Bytes,
        filename: &str,
        content_type: Option<&str>,
        metadata: serde_json::Value,
    ) -> Result<Blob, StorageError> {
        self.create_and_upload_exact(db, data, filename, content_type, with_identified(metadata))
            .await
    }

    /// Like [`Self::create_and_upload`], storing `metadata` as given: an attachment built
    /// with `metadata:` replaces the blob's metadata.
    pub async fn create_and_upload_exact(
        &self,
        db: &sqlx::PgPool,
        data: Bytes,
        filename: &str,
        content_type: Option<&str>,
        metadata: serde_json::Value,
    ) -> Result<Blob, StorageError> {
        let key = generate_key();
        let checksum = checksum(&data);

        self.put(&self.default_service, &key, data.clone()).await?;

        let blob = Blob::insert(
            db,
            NewBlob {
                key: &key,
                filename,
                content_type,
                metadata: &metadata,
                service_name: &self.default_service,
                byte_size: i64::try_from(data.len()).unwrap_or(i64::MAX),
                checksum: &checksum,
            },
        )
        .await;

        match blob {
            Ok(blob) => Ok(blob),
            Err(err) => {
                // Don't leave an orphaned file behind.
                let _ = self.delete_key(&self.default_service, &key).await;
                Err(err.into())
            }
        }
    }

    pub async fn download(&self, blob: &Blob) -> Result<Bytes, StorageError> {
        let service = self.service(&blob.service_name)?;
        let data = service
            .store
            .get(&object_path(service, &blob.key))
            .await?
            .bytes()
            .await?;

        if blob
            .checksum
            .as_deref()
            .is_some_and(|c| c != checksum(&data))
        {
            return Err(StorageError::Integrity(blob.key.clone()));
        }

        Ok(data)
    }

    /// The file at `key` of a service, without a checksum check (the disk controller).
    pub async fn download_key(&self, service_name: &str, key: &str) -> Result<Bytes, StorageError> {
        let service = self.service(service_name)?;
        Ok(service
            .store
            .get(&object_path(service, key))
            .await?
            .bytes()
            .await?)
    }

    /// Whether a service stores files on local disk (`ActiveStorage::Service::DiskService`).
    #[must_use]
    pub fn is_disk(&self, service_name: &str) -> bool {
        self.services
            .get(service_name)
            .is_some_and(|s| s.sharded_disk_layout)
    }

    /// Deletes the stored file (the database row is left to the caller, as `Blob#delete`).
    pub async fn delete(&self, blob: &Blob) -> Result<(), StorageError> {
        self.delete_key(&blob.service_name, &blob.key).await
    }

    async fn put(&self, service_name: &str, key: &str, data: Bytes) -> Result<(), StorageError> {
        let service = self.service(service_name)?;
        service
            .store
            .put(&object_path(service, key), PutPayload::from_bytes(data))
            .await?;
        Ok(())
    }

    async fn delete_key(&self, service_name: &str, key: &str) -> Result<(), StorageError> {
        let service = self.service(service_name)?;
        match service.store.delete(&object_path(service, key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn service(&self, name: &str) -> Result<&Service, StorageError> {
        self.services
            .get(name)
            .ok_or_else(|| StorageError::UnknownService(name.to_owned()))
    }
}

fn object_path(service: &Service, key: &str) -> ObjectPath {
    if service.sharded_disk_layout && key.len() >= 4 {
        ObjectPath::from(format!("{}/{}/{key}", &key[0..2], &key[2..4]))
    } else {
        ObjectPath::from(key)
    }
}

/// `SecureRandom.base36(28)`: 28 lowercase alphanumeric characters.
#[must_use]
pub fn generate_key() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..28)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Base64 MD5, as stored in `active_storage_blobs.checksum`.
#[must_use]
pub fn checksum(data: &[u8]) -> String {
    STANDARD.encode(Md5::digest(data))
}

fn with_identified(metadata: serde_json::Value) -> serde_json::Value {
    let mut map = match metadata {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    map.entry("identified").or_insert(true.into());
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_checksums_match_rails_format() {
        let key = generate_key();
        assert_eq!(key.len(), 28);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        );
        // Digest::MD5.base64digest("hello")
        assert_eq!(checksum(b"hello"), "XUFAKrxLKna5cZ2REBfFkg==");
    }

    #[tokio::test]
    async fn disk_layout_matches_active_storage() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            default_service: config::DISK.into(),
            services: vec![(
                config::DISK.into(),
                ServiceConfig::Disk {
                    root: dir.path().to_owned(),
                },
            )],
        };
        let storage = Storage::new(&config).unwrap();

        storage
            .put(config::DISK, "abcdefghij", Bytes::from_static(b"pdf"))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("ab/cd/abcdefghij")).unwrap(),
            b"pdf"
        );
    }
}
