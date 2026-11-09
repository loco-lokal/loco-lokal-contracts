//! `cookies.encrypted`: `ActiveSupport::MessageEncryptor` with AES-256-GCM, as Rails' cookie
//! jar uses it (`authenticated encrypted cookie` salt, JSON serializer).
//!
//! A cookie value is `base64(ciphertext)--base64(iv)--base64(auth_tag)`. The plaintext is
//! `{"_rails":{"message":base64(json(value)),"exp":…,"pur":"cookie.<name>"}}`, with `exp`
//! only when the cookie has an expiry.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;
const SEPARATOR: &str = "--";

#[derive(Clone)]
pub struct CookieEncryptor {
    key: Key<Aes256Gcm>,
}

impl CookieEncryptor {
    /// `key` is `key_generator.generate_key("authenticated encrypted cookie", 32)`.
    #[must_use]
    pub fn new(key: &[u8]) -> Self {
        Self {
            key: Key::<Aes256Gcm>::try_from(key).expect("32-byte key"),
        }
    }

    /// The value of `cookies.encrypted[name] = { value:, expires: }`, before URL escaping.
    #[must_use]
    pub fn encrypt(&self, name: &str, value: &Value, expires: Option<DateTime<Utc>>) -> String {
        let mut meta = Map::new();
        meta.insert("message".into(), STANDARD.encode(value.to_string()).into());
        if let Some(exp) = expires {
            meta.insert(
                "exp".into(),
                exp.to_rfc3339_opts(SecondsFormat::Millis, true).into(),
            );
        }
        meta.insert("pur".into(), format!("cookie.{name}").into());
        let mut envelope = Map::new();
        envelope.insert("_rails".into(), Value::Object(meta));
        let plaintext = Value::Object(envelope).to_string();

        let iv: [u8; IV_LEN] = rand::random();
        let mut sealed = Aes256Gcm::new(&self.key)
            .encrypt(
                &Nonce::try_from(iv.as_slice()).expect("12-byte iv"),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: b"",
                },
            )
            .expect("AES-GCM encryption cannot fail for in-memory data");
        let tag = sealed.split_off(sealed.len() - TAG_LEN);
        format!(
            "{}{SEPARATOR}{}{SEPARATOR}{}",
            STANDARD.encode(sealed),
            STANDARD.encode(iv),
            STANDARD.encode(tag)
        )
    }

    /// `cookies.encrypted[name]`: `None` for a tampered, expired or misnamed cookie.
    #[must_use]
    pub fn decrypt(&self, name: &str, cookie: &str) -> Option<Value> {
        self.decrypt_at(name, cookie, Utc::now())
    }

    #[must_use]
    pub fn decrypt_at(&self, name: &str, cookie: &str, now: DateTime<Utc>) -> Option<Value> {
        let mut parts = cookie.split(SEPARATOR);
        let (data, iv, tag) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let (mut sealed, iv, tag) = (
            STANDARD.decode(data).ok()?,
            STANDARD.decode(iv).ok()?,
            STANDARD.decode(tag).ok()?,
        );
        if iv.len() != IV_LEN || tag.len() != TAG_LEN {
            return None;
        }
        sealed.extend_from_slice(&tag);
        let plaintext = Aes256Gcm::new(&self.key)
            .decrypt(
                &Nonce::try_from(iv.as_slice()).ok()?,
                Payload {
                    msg: &sealed,
                    aad: b"",
                },
            )
            .ok()?;
        let value: Value = serde_json::from_slice(&plaintext).ok()?;
        let Some(meta) = value.get("_rails").and_then(Value::as_object) else {
            // Cookies written before metadata was added carry the bare value.
            return Some(value);
        };
        if meta.get("pur").and_then(Value::as_str) != Some(&format!("cookie.{name}")) {
            return None;
        }
        if let Some(exp) = meta.get("exp").and_then(Value::as_str) {
            let exp = DateTime::parse_from_rfc3339(exp).ok()?;
            if now >= exp {
                return None;
            }
        }
        if let Some(message) = meta.get("message").and_then(Value::as_str) {
            return serde_json::from_slice(&STANDARD.decode(message).ok()?).ok();
        }
        meta.get("data").cloned()
    }
}
