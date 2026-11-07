//! Shared application state and its construction at boot.

use std::sync::{Arc, RwLock};

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
pub const APP_URL_KEY: &str = "app_url";
pub const ESIGN_CERTS_KEY: &str = "esign_certs";

/// `Docuseal::DEFAULT_APP_URL` without `APP_URL`.
const FALLBACK_APP_URL: &str = "http://localhost:3000";

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub secrets: RailsSecrets,
    pub storage: Storage,
    pub views: Views,
    pub mailer: Mailer,
    /// `Docuseal.default_url_options` as an origin (`http://host[:port]`).
    default_url: Arc<RwLock<String>>,
    pub rate_limit: crate::rate_limit::RateLimit,
    /// Outgoing HTTP (webhooks): `open_timeout` 8 s, no redirects, as Faraday does.
    pub http: reqwest::Client,
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

        let state = Self {
            db,
            config: Arc::new(config),
            secrets,
            storage,
            views,
            mailer,
            default_url: Arc::new(RwLock::new(FALLBACK_APP_URL.to_owned())),
            rate_limit: crate::rate_limit::RateLimit::default(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .context("building the HTTP client")?,
        };
        state.refresh_default_url().await?;
        Ok(state)
    }

    /// The app's origin for absolute links, e.g. `http://localhost:3000`.
    #[must_use]
    pub fn default_url(&self) -> String {
        self.default_url.read().expect("lock").clone()
    }

    /// `Docuseal.refresh_default_url_options!`: `APP_URL`, else the first account's stored
    /// `app_url`, else `http://localhost:3000`.
    pub async fn refresh_default_url(&self) -> anyhow::Result<()> {
        let value = match &self.config.app_url {
            Some(url) => url.clone(),
            None => match loco_db::first_encrypted_config(&self.db, APP_URL_KEY).await? {
                Some(row) => match decrypt_json(&self.secrets, &row.value)? {
                    serde_json::Value::String(url) => url,
                    _ => FALLBACK_APP_URL.to_owned(),
                },
                None => FALLBACK_APP_URL.to_owned(),
            },
        };

        let origin = url::Url::parse(&value)
            .ok()
            .filter(url::Url::has_host)
            .map_or_else(
                || FALLBACK_APP_URL.to_owned(),
                |url| url.origin().ascii_serialization(),
            );
        *self.default_url.write().expect("lock") = origin;
        Ok(())
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

pub(crate) fn decrypt_json(
    secrets: &RailsSecrets,
    encrypted: &str,
) -> anyhow::Result<serde_json::Value> {
    let json = secrets.encryptor.decrypt(encrypted)?;
    Ok(serde_json::from_str(&json)?)
}
