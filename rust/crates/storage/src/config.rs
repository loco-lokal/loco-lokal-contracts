//! Service configuration matching `config/storage.yml`, the production service choice
//! (`config/environments/production.rb`), and the account-level override stored in
//! `encrypted_configs` under the `active_storage` key (`lib/load_active_storage_configs.rb`).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Service names as used in `active_storage_blobs.service_name`.
pub const DISK: &str = "disk";
pub const AWS_S3: &str = "aws_s3";
pub const GOOGLE: &str = "google";
pub const AZURE: &str = "azure";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceConfig {
    Disk {
        root: PathBuf,
    },
    S3 {
        access_key_id: Option<String>,
        secret_access_key: Option<String>,
        region: String,
        bucket: String,
        endpoint: Option<String>,
    },
    Gcs {
        /// Service-account JSON.
        credentials: String,
        bucket: String,
    },
    Azure {
        account: String,
        access_key: Option<String>,
        container: String,
    },
}

#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Service used for new uploads.
    pub default_service: String,
    /// Every service that is configured, by name. Old blobs keep the service they were
    /// uploaded to, so all configured services stay readable.
    pub services: Vec<(String, ServiceConfig)>,
}

impl StorageConfig {
    /// Configuration from environment variables, as Rails reads them in production.
    #[must_use]
    pub fn from_env(workdir: &Path) -> Self {
        Self::from_lookup(workdir, |key| {
            std::env::var(key).ok().filter(|v| !v.is_empty())
        })
    }

    pub fn from_lookup(workdir: &Path, var: impl Fn(&str) -> Option<String>) -> Self {
        let mut services = vec![(
            DISK.to_owned(),
            ServiceConfig::Disk {
                root: workdir.join("attachments"),
            },
        )];

        if let Some(bucket) = var("S3_ATTACHMENTS_BUCKET") {
            services.push((
                AWS_S3.to_owned(),
                ServiceConfig::S3 {
                    access_key_id: var("AWS_ACCESS_KEY_ID"),
                    secret_access_key: var("AWS_SECRET_ACCESS_KEY"),
                    region: var("AWS_REGION").unwrap_or_else(|| "us-east-1".into()),
                    bucket,
                    endpoint: var("S3_ENDPOINT"),
                },
            ));
        }
        if let Some(bucket) = var("GCS_BUCKET") {
            services.push((
                GOOGLE.to_owned(),
                ServiceConfig::Gcs {
                    credentials: var("GCS_CREDENTIALS").unwrap_or_else(|| "{}".into()),
                    bucket,
                },
            ));
        }
        if let Some(container) = var("AZURE_CONTAINER") {
            services.push((
                AZURE.to_owned(),
                ServiceConfig::Azure {
                    account: var("AZURE_STORAGE_ACCOUNT_NAME").unwrap_or_default(),
                    access_key: var("AZURE_STORAGE_ACCESS_KEY"),
                    container,
                },
            ));
        }

        // Same precedence as production.rb.
        let default_service = [AWS_S3, GOOGLE, AZURE]
            .into_iter()
            .find(|name| services.iter().any(|(n, _)| n == name))
            .unwrap_or(DISK)
            .to_owned();

        Self {
            default_service,
            services,
        }
    }

    /// Whether storage is configured through the environment, in which case Rails ignores
    /// the database override.
    #[must_use]
    pub fn is_env_configured(&self) -> bool {
        self.default_service != DISK
    }

    /// Applies the decrypted `active_storage` encrypted config
    /// (`{"service": "...", "configs": {...}}`) the way `LoadActiveStorageConfigs` does.
    pub fn apply_database_override(&mut self, value: &Value) -> Result<(), String> {
        if self.is_env_configured() {
            return Ok(());
        }

        let service = value
            .get("service")
            .and_then(Value::as_str)
            .ok_or("missing service")?;
        let configs = value.get("configs").cloned().unwrap_or(Value::Null);
        let get = |key: &str| {
            configs
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };

        let config = match service {
            DISK => return Ok(()),
            AWS_S3 => ServiceConfig::S3 {
                access_key_id: get("access_key_id"),
                secret_access_key: get("secret_access_key"),
                region: get("region").unwrap_or_else(|| "us-east-1".into()),
                bucket: get("bucket").ok_or("missing bucket")?,
                endpoint: get("endpoint"),
            },
            GOOGLE => ServiceConfig::Gcs {
                credentials: get("credentials").unwrap_or_else(|| "{}".into()),
                bucket: get("bucket").ok_or("missing bucket")?,
            },
            AZURE => ServiceConfig::Azure {
                account: get("storage_account_name").unwrap_or_default(),
                access_key: get("storage_access_key"),
                container: get("container").ok_or("missing container")?,
            },
            other => return Err(format!("unknown storage service {other}")),
        };

        self.services.retain(|(name, _)| name != service);
        self.services.push((service.to_owned(), config));
        service.clone_into(&mut self.default_service);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_to_disk_under_workdir() {
        let config = StorageConfig::from_lookup(Path::new("/data"), |_| None);

        assert_eq!(config.default_service, DISK);
        assert_eq!(
            config.services,
            vec![(
                DISK.into(),
                ServiceConfig::Disk {
                    root: "/data/attachments".into()
                }
            )]
        );
    }

    #[test]
    fn env_s3_wins_and_ignores_database_override() {
        let mut config = StorageConfig::from_lookup(Path::new("."), |k| {
            (k == "S3_ATTACHMENTS_BUCKET").then(|| "docs".to_owned())
        });
        config
            .apply_database_override(&json!({"service": "azure", "configs": {"container": "c"}}))
            .unwrap();

        assert_eq!(config.default_service, AWS_S3);
        assert_eq!(config.services.len(), 2);
    }

    #[test]
    fn database_override_selects_service() {
        let mut config = StorageConfig::from_lookup(Path::new("."), |_| None);
        config
            .apply_database_override(&json!({
                "service": "aws_s3",
                "configs": {"bucket": "b", "region": "eu-central-1", "endpoint": "https://s3.example"}
            }))
            .unwrap();

        assert_eq!(config.default_service, AWS_S3);
        assert!(config.services.iter().any(|(n, _)| n == DISK));
    }
}
