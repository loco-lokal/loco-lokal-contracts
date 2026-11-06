//! Active Record encryption (`encrypts :value`), non-deterministic mode.
//!
//! Stored format: `{"p":b64(ciphertext),"h":{"iv":b64(12 bytes),"at":b64(16 bytes),"c":true?}}`.
//! The payload is zlib-deflated first when the plaintext exceeds 140 bytes. The key is
//! `PBKDF2-HMAC-SHA1(primary_key, key_derivation_salt, 2^16, 32)`; this app's config sets
//! `primary_key = encryption_secret[0, 32]` and
//! `key_derivation_salt = SHA256.hexdigest(encryption_secret)`.

use std::io::{Read, Write};

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::key_generator::{Digest, KeyGenerator};

const KEY_DERIVATION_ITERATIONS: u32 = 1 << 16;
const COMPRESSION_THRESHOLD: usize = 140;
const IV_LEN: usize = 12;
const AUTH_TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncryptionError {
    #[error("value is not an Active Record encrypted message")]
    Encoding,
    #[error("unable to decrypt value")]
    Decryption,
}

#[derive(Clone)]
pub struct Encryptor {
    /// Candidate keys; the first one encrypts, all are tried when decrypting
    /// (Rails accepts a list of primary keys for rotation).
    keys: Vec<Key<Aes256Gcm>>,
}

impl Encryptor {
    /// Keys as configured in `config/environments/production.rb`.
    #[must_use]
    pub fn from_encryption_secret(encryption_secret: &str) -> Self {
        let primary_key: String = encryption_secret.chars().take(32).collect();
        let salt = hex::encode(Sha256::digest(encryption_secret));

        Self::from_primary_keys(&[primary_key.as_str()], &salt)
    }

    #[must_use]
    pub fn from_primary_keys(primary_keys: &[&str], key_derivation_salt: &str) -> Self {
        let keys = primary_keys
            .iter()
            .map(|primary| {
                let raw =
                    KeyGenerator::new(primary.as_bytes(), KEY_DERIVATION_ITERATIONS, Digest::Sha1)
                        .generate_key(key_derivation_salt.as_bytes(), 32);
                Key::<Aes256Gcm>::try_from(raw.as_slice()).expect("32-byte key")
            })
            .collect();

        Self { keys }
    }

    /// The derived AES key used for encryption (for diagnostics and tests).
    #[must_use]
    pub fn primary_key_bytes(&self) -> &[u8] {
        self.keys[0].as_slice()
    }

    #[must_use]
    pub fn encrypt(&self, clear_text: &str) -> String {
        let (data, compressed) = if clear_text.len() > COMPRESSION_THRESHOLD {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder
                .write_all(clear_text.as_bytes())
                .expect("in-memory write");
            (encoder.finish().expect("in-memory write"), true)
        } else {
            (clear_text.as_bytes().to_vec(), false)
        };

        let iv: [u8; IV_LEN] = rand::random();
        let cipher = Aes256Gcm::new(&self.keys[0]);
        let mut sealed = cipher
            .encrypt(
                &Nonce::try_from(iv.as_slice()).expect("12-byte iv"),
                Payload {
                    msg: &data,
                    aad: b"",
                },
            )
            .expect("AES-GCM encryption cannot fail for in-memory data");
        let auth_tag = sealed.split_off(sealed.len() - AUTH_TAG_LEN);

        let mut headers = Map::new();
        headers.insert("iv".into(), STANDARD.encode(iv).into());
        headers.insert("at".into(), STANDARD.encode(auth_tag).into());
        if compressed {
            headers.insert("c".into(), Value::Bool(true));
        }

        json!({ "p": STANDARD.encode(sealed), "h": headers }).to_string()
    }

    pub fn decrypt(&self, encrypted: &str) -> Result<String, EncryptionError> {
        let message: Value =
            serde_json::from_str(encrypted).map_err(|_| EncryptionError::Encoding)?;
        let payload = decode_field(message.get("p"))?;
        let headers = message.get("h").ok_or(EncryptionError::Decryption)?;
        let iv = decode_field(headers.get("iv"))?;
        let auth_tag = decode_field(headers.get("at"))?;
        let compressed = headers.get("c").and_then(Value::as_bool).unwrap_or(false);

        if iv.len() != IV_LEN || auth_tag.len() != AUTH_TAG_LEN {
            return Err(EncryptionError::Decryption);
        }

        let mut sealed = payload;
        sealed.extend_from_slice(&auth_tag);

        let data = self
            .keys
            .iter()
            .find_map(|key| {
                Aes256Gcm::new(key)
                    .decrypt(
                        &Nonce::try_from(iv.as_slice()).expect("checked length"),
                        Payload {
                            msg: &sealed,
                            aad: b"",
                        },
                    )
                    .ok()
            })
            .ok_or(EncryptionError::Decryption)?;

        let data = if compressed {
            let mut inflated = Vec::new();
            flate2::read::ZlibDecoder::new(data.as_slice())
                .read_to_end(&mut inflated)
                .map_err(|_| EncryptionError::Decryption)?;
            inflated
        } else {
            data
        };

        String::from_utf8(data).map_err(|_| EncryptionError::Decryption)
    }

    /// Whether `value` looks like an encrypted message (`Encryptor#encrypted?`).
    #[must_use]
    pub fn is_encrypted(&self, value: &str) -> bool {
        self.decrypt(value).is_ok()
    }
}

fn decode_field(value: Option<&Value>) -> Result<Vec<u8>, EncryptionError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or(EncryptionError::Decryption)?;
    STANDARD
        .decode(text)
        .map_err(|_| EncryptionError::Decryption)
}
