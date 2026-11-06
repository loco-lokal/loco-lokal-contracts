//! Shared application state and its construction at boot.

use std::sync::Arc;

use anyhow::Context;
use loco_db::PgPool;
use loco_mailer::{Delivery, Mailer};
use loco_rails_compat::RailsSecrets;
use loco_storage::{Storage, StorageConfig};
use loco_views::{Views, ViewsConfig};

use crate::config::Config;

/// `EncryptedConfig` keys (`app/models/encrypted_config.rb`).
pub const FILES_STORAGE_KEY: &str = "active_storage";
pub const EMAIL_SMTP_KEY: &str = "action_mailer_smtp";

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub secrets: RailsSecrets,
    pub storage: Storage,
    pub views: Views,
    pub mailer: Mailer,
}

impl AppState {
    pub async fn build(config: Config, db: PgPool) -> anyhow::Result<Self> {
        let secrets =
            RailsSecrets::new(&config.secret_key_base, config.encryption_secret.as_deref());

        let storage = Storage::new(&storage_config(&config, &db, &secrets).await?)
            .context("configuring file storage")?;

        let views = Views::new(&ViewsConfig::from_repo_root(
            &config.app_root,
            !config.production,
        ))
        .context("loading views")?;

        let mailer =
            Mailer::new(mail_delivery(&db, &secrets).await?).context("configuring SMTP")?;

        Ok(Self {
            db,
            config: Arc::new(config),
            secrets,
            storage,
            views,
            mailer,
        })
    }
}

/// Environment configuration, overridden by the stored settings unless storage is
/// configured through the environment (`LoadActiveStorageConfigs`).
async fn storage_config(
    config: &Config,
    db: &PgPool,
    secrets: &RailsSecrets,
) -> anyhow::Result<StorageConfig> {
    let mut storage = StorageConfig::from_env(&config.workdir);

    if config.production
        && !storage.is_env_configured()
        && let Some(row) = loco_db::first_encrypted_config(db, FILES_STORAGE_KEY).await?
    {
        let value = decrypt_json(secrets, &row.value).context("decrypting storage settings")?;
        storage
            .apply_database_override(&value)
            .map_err(|e| anyhow::anyhow!("stored storage settings: {e}"))?;
    }

    Ok(storage)
}

async fn mail_delivery(db: &PgPool, secrets: &RailsSecrets) -> anyhow::Result<Delivery> {
    if let Some(delivery) =
        Delivery::from_env_lookup(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
    {
        return Ok(delivery);
    }

    match loco_db::first_encrypted_config(db, EMAIL_SMTP_KEY).await? {
        Some(row) => {
            let value = decrypt_json(secrets, &row.value).context("decrypting SMTP settings")?;
            Ok(Delivery::from_account_config(&value, &row.account_name)?)
        }
        None => Ok(Delivery::Discard),
    }
}

fn decrypt_json(secrets: &RailsSecrets, encrypted: &str) -> anyhow::Result<serde_json::Value> {
    let json = secrets.encryptor.decrypt(encrypted)?;
    Ok(serde_json::from_str(&json)?)
}
