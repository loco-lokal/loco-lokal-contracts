//! Compatibility with data that Rails produced, so the Rust backend can read and write the
//! same database and keep links that have already been sent working:
//!
//! - `encryption`: Active Record `encrypts` columns (AES-256-GCM in a JSON envelope)
//! - `message_verifier`: `ActiveSupport::MessageVerifier` (signed ids and blob ids)
//! - `password`: Devise bcrypt password hashes
//! - `csrf`: masked per-session authenticity tokens, as `form_authenticity_token` produces
//!
//! [`RailsSecrets`] derives every key from `SECRET_KEY_BASE` / `ENCRYPTION_SECRET` the way
//! `config/environments/production.rb` and the Rails railties do.

pub mod csrf;
pub mod encryption;
pub mod key_generator;
pub mod message_verifier;
pub mod password;

use encryption::Encryptor;
use key_generator::{Digest, KeyGenerator};
use message_verifier::MessageVerifier;
use sha2::{Digest as _, Sha256};

/// Iterations used by `Rails.application.key_generator`.
const APP_KEY_GENERATOR_ITERATIONS: u32 = 1000;

/// Keys derived from the application secrets, built once at boot.
#[derive(Clone)]
pub struct RailsSecrets {
    /// `ActiveRecord::Encryption` for `encrypts` columns.
    pub encryptor: Encryptor,
    /// `ApplicationRecord.signed_id_verifier`, also used directly by the app for
    /// attachment and blob-proxy links (`config/initializers/active_storage.rb`).
    pub signed_id_verifier: MessageVerifier,
    /// `ActiveStorage.verifier` (`ActiveStorage::Blob.find_signed`).
    pub active_storage_verifier: MessageVerifier,
}

impl RailsSecrets {
    /// `encryption_secret` is `ENCRYPTION_SECRET`; when absent Rails uses
    /// `SHA256.hexdigest(SECRET_KEY_BASE)`.
    #[must_use]
    pub fn new(secret_key_base: &str, encryption_secret: Option<&str>) -> Self {
        let encryption_secret = encryption_secret.filter(|s| !s.is_empty()).map_or_else(
            || hex::encode(Sha256::digest(secret_key_base)),
            str::to_owned,
        );

        let app_keys = KeyGenerator::new(
            secret_key_base.as_bytes(),
            APP_KEY_GENERATOR_ITERATIONS,
            Digest::Sha256,
        );

        Self {
            encryptor: Encryptor::from_encryption_secret(&encryption_secret),
            signed_id_verifier: MessageVerifier::new(
                app_keys.generate_key(b"active_record/signed_id", 64),
                Digest::Sha256,
                true,
            ),
            active_storage_verifier: MessageVerifier::new(
                app_keys.generate_key(b"ActiveStorage", 64),
                Digest::Sha1,
                false,
            ),
        }
    }
}

/// Purpose string for `record.signed_id(purpose:)`, which Rails combines with the
/// model's underscored base class name, e.g. `("submitter", "download_completed")`
/// gives `"submitter/download_completed"`.
#[must_use]
pub fn signed_id_purpose(model: &str, purpose: Option<&str>) -> String {
    match purpose {
        Some(purpose) => format!("{model}/{purpose}"),
        None => model.to_owned(),
    }
}
