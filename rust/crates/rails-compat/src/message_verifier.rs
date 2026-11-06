//! `ActiveSupport::MessageVerifier` with the JSON serializer.
//!
//! A message is `base64(json) + "--" + hex(HMAC(secret, base64(json)))`. When a purpose or
//! expiry is given, the value is wrapped as `{"_rails":{"data":…,"exp":…,"pur":…}}`, with
//! `exp` as ISO 8601 UTC with milliseconds. URL-safe verifiers use unpadded URL-safe base64.

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::key_generator::Digest;

const SEPARATOR: &str = "--";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("malformed or tampered message")]
    Invalid,
    #[error("message purpose does not match")]
    Purpose,
    #[error("message has expired")]
    Expired,
}

#[derive(Clone)]
pub struct MessageVerifier {
    secret: Vec<u8>,
    digest: Digest,
    url_safe: bool,
}

impl MessageVerifier {
    #[must_use]
    pub fn new(secret: Vec<u8>, digest: Digest, url_safe: bool) -> Self {
        Self {
            secret,
            digest,
            url_safe,
        }
    }

    #[must_use]
    pub fn generate(
        &self,
        data: &Value,
        purpose: Option<&str>,
        expires_at: Option<DateTime<Utc>>,
    ) -> String {
        let value = if purpose.is_some() || expires_at.is_some() {
            let mut meta = Map::new();
            meta.insert("data".into(), data.clone());
            if let Some(exp) = expires_at {
                meta.insert(
                    "exp".into(),
                    exp.to_rfc3339_opts(SecondsFormat::Millis, true).into(),
                );
            }
            if let Some(purpose) = purpose {
                meta.insert("pur".into(), purpose.into());
            }
            let mut envelope = Map::new();
            envelope.insert("_rails".into(), Value::Object(meta));
            Value::Object(envelope)
        } else {
            data.clone()
        };

        let encoded = self.encode(value.to_string().as_bytes());
        let digest = hex::encode(self.hmac(encoded.as_bytes()));

        format!("{encoded}{SEPARATOR}{digest}")
    }

    /// Verifies `message` and returns its data. `purpose` must equal the purpose the message
    /// was generated with (`None` only matches messages generated without one).
    pub fn verify(&self, message: &str, purpose: Option<&str>) -> Result<Value, VerifyError> {
        self.verify_at(message, purpose, Utc::now())
    }

    pub fn verify_at(
        &self,
        message: &str,
        purpose: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Value, VerifyError> {
        let (encoded, digest) = message.rsplit_once(SEPARATOR).ok_or(VerifyError::Invalid)?;
        let digest = hex::decode(digest).map_err(|_| VerifyError::Invalid)?;

        if !bool::from(self.hmac(encoded.as_bytes()).ct_eq(&digest)) {
            return Err(VerifyError::Invalid);
        }

        let json = self.decode(encoded).ok_or(VerifyError::Invalid)?;
        let value: Value = serde_json::from_slice(&json).map_err(|_| VerifyError::Invalid)?;

        let Some(meta) = value.get("_rails").and_then(Value::as_object) else {
            return if purpose.is_none() {
                Ok(value)
            } else {
                Err(VerifyError::Purpose)
            };
        };

        if meta.get("pur").and_then(Value::as_str) != purpose {
            return Err(VerifyError::Purpose);
        }

        if let Some(exp) = meta.get("exp").and_then(Value::as_str) {
            let exp = DateTime::parse_from_rfc3339(exp).map_err(|_| VerifyError::Invalid)?;
            if now >= exp {
                return Err(VerifyError::Expired);
            }
        }

        Ok(meta.get("data").cloned().unwrap_or(Value::Null))
    }

    fn hmac(&self, data: &[u8]) -> Vec<u8> {
        match self.digest {
            Digest::Sha1 => {
                let mut mac = Hmac::<Sha1>::new_from_slice(&self.secret).expect("any key size");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
            Digest::Sha256 => {
                let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("any key size");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
        }
    }

    fn encode(&self, data: &[u8]) -> String {
        if self.url_safe {
            URL_SAFE_NO_PAD.encode(data)
        } else {
            STANDARD.encode(data)
        }
    }

    fn decode(&self, data: &str) -> Option<Vec<u8>> {
        if self.url_safe {
            URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')).ok()
        } else {
            STANDARD.decode(data).ok()
        }
    }
}
