//! Runtime configuration, read from the same environment variables as the Rails app
//! (`config/dotenv.rb`, `config/environments/production.rb`).

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, bail};

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub database_pool_size: u32,
    pub secret_key_base: String,
    /// `ENCRYPTION_SECRET`, falling back to SHA-256(`SECRET_KEY_BASE`) in the
    /// `rails-compat` crate, as in Rails production config.
    pub encryption_secret: Option<String>,
    pub workdir: PathBuf,
    pub app_url: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let workdir = PathBuf::from(var("WORKDIR").unwrap_or_else(|| ".".into()));

        // Like Rails: without SECRET_KEY_BASE, load `$WORKDIR/docuseal.env` if it exists.
        // Generating that file on first boot is handled at cutover.
        if var("SECRET_KEY_BASE").is_none() {
            let dotenv = workdir.join("docuseal.env");
            if dotenv.exists() {
                dotenvy::from_path(&dotenv)
                    .with_context(|| format!("loading {}", dotenv.display()))?;
            }
        }

        let Some(secret_key_base) = var("SECRET_KEY_BASE") else {
            bail!("SECRET_KEY_BASE is not set");
        };

        let port: u16 = var("PORT")
            .map_or(Ok(8080), |p| p.parse())
            .context("invalid PORT")?;
        let host = var("BIND_HOST").unwrap_or_else(|| "127.0.0.1".into());

        Ok(Self {
            bind: format!("{host}:{port}")
                .parse()
                .context("invalid BIND_HOST/PORT")?,
            database_url: database_url()?,
            database_pool_size: var("DATABASE_POOL")
                .map_or(Ok(10), |n| n.parse())
                .context("invalid DATABASE_POOL")?,
            secret_key_base,
            encryption_secret: var("ENCRYPTION_SECRET"),
            workdir,
            app_url: var("APP_URL"),
        })
    }
}

/// `DATABASE_URL`, or the split `DATABASE_HOST`/`PORT`/`USER`/`PASSWORD`/`NAME` variables
/// that `config/dotenv.rb` produces for hosts containing underscores.
fn database_url() -> anyhow::Result<String> {
    if let Some(url) = var("DATABASE_URL") {
        return Ok(url);
    }

    let Some(host) = var("DATABASE_HOST") else {
        bail!("DATABASE_URL is not set (only PostgreSQL is supported)");
    };
    let port = var("DATABASE_PORT").unwrap_or_else(|| "5432".into());
    let user = var("DATABASE_USER").unwrap_or_default();
    let password = var("DATABASE_PASSWORD").unwrap_or_default();
    let name = var("DATABASE_NAME").unwrap_or_default();

    Ok(format!(
        "postgresql://{user}:{password}@{host}:{port}/{name}"
    ))
}

/// Environment variable, treating empty values as unset (Rails uses `.to_s.empty?`).
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}
