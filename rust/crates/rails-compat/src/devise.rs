//! Devise tokens and devise-two-factor one-time passwords.
//!
//! - [`TokenGenerator`] is `Devise.token_generator`: the database stores
//!   `HMAC-SHA256(key, raw)` while the raw token goes out by email. The key is
//!   `KeyGenerator(secret_key_base, 2**16, SHA1).generate_key("Devise <column>")`.
//! - [`friendly_token`] is `Devise.friendly_token`.
//! - [`Totp`] is the `ROTP::TOTP` that devise-two-factor uses: SHA1, 6 digits, 30-second
//!   steps. A code verifies within `otp_allowed_drift` (60 s) either side, and only once
//!   per step (`consumed_timestep`).

use std::fmt::Write as _;

use base64::{Engine, engine::general_purpose::URL_SAFE};
use hmac::{Hmac, KeyInit, Mac};
use rand::RngExt as _;
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::key_generator::{Digest, KeyGenerator};

const DEVISE_KEY_ITERATIONS: u32 = 1 << 16;

#[derive(Clone)]
pub struct TokenGenerator {
    keys: KeyGenerator,
}

impl TokenGenerator {
    #[must_use]
    pub fn new(secret_key_base: &str) -> Self {
        Self {
            keys: KeyGenerator::new(
                secret_key_base.as_bytes(),
                DEVISE_KEY_ITERATIONS,
                // Devise builds its key generator before Rails applies the SHA256 default.
                Digest::Sha1,
            ),
        }
    }

    /// `Devise.token_generator.digest(klass, column, raw)`.
    #[must_use]
    pub fn digest(&self, column: &str, raw: &str) -> String {
        let key = self
            .keys
            .generate_key(format!("Devise {column}").as_bytes(), 64);
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("any key length");
        mac.update(raw.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Returns `(raw, digest)`. Devise retries on a digest collision; callers that store
    /// the digest under a unique index can do the same.
    #[must_use]
    pub fn generate(&self, column: &str) -> (String, String) {
        let raw = friendly_token();
        let digest = self.digest(column, &raw);
        (raw, digest)
    }
}

/// `Devise.friendly_token(20)`: URL-safe base64 of 15 random bytes with the look-alike
/// characters `lIO0` replaced by `sxyz`.
#[must_use]
pub fn friendly_token() -> String {
    let bytes: [u8; 15] = rand::rng().random();
    URL_SAFE
        .encode(bytes)
        .chars()
        .map(|c| match c {
            'l' => 's',
            'I' => 'x',
            'O' => 'y',
            '0' => 'z',
            c => c,
        })
        .collect()
}

const OTP_INTERVAL: i64 = 30;
/// `User.otp_allowed_drift`.
pub const OTP_ALLOWED_DRIFT: i64 = 60;
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub struct Totp {
    key: Vec<u8>,
}

impl Totp {
    /// `secret` is the Base32 `users.otp_secret` (after decryption). Returns `None` when
    /// it is not valid Base32.
    #[must_use]
    pub fn new(secret: &str) -> Option<Self> {
        base32_decode(secret).map(|key| Self { key })
    }

    /// `User.generate_otp_secret`: `ROTP::Base32.random(20)`, 32 characters.
    #[must_use]
    pub fn generate_secret() -> String {
        let bytes: [u8; 20] = rand::rng().random();
        base32_encode(&bytes)
    }

    /// `totp.at(unix_time)`.
    #[must_use]
    pub fn at(&self, unix_time: i64) -> String {
        self.code_for_step(unix_time.div_euclid(OTP_INTERVAL))
    }

    fn code_for_step(&self, step: i64) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(&self.key).expect("any key length");
        mac.update(&step.to_be_bytes());
        let hash = mac.finalize().into_bytes();
        let offset = usize::from(hash[hash.len() - 1] & 0x0f);
        let code = u32::from_be_bytes([
            hash[offset] & 0x7f,
            hash[offset + 1],
            hash[offset + 2],
            hash[offset + 3],
        ]) % 1_000_000;
        format!("{code:06}")
    }

    /// `validate_and_consume_otp!` without the save: returns the timestep to store in
    /// `consumed_timestep` when `code` is valid at `now` and its step is later than
    /// `consumed_timestep`. Whitespace in the code is ignored.
    #[must_use]
    pub fn verify(&self, code: &str, now: i64, consumed_timestep: Option<i64>) -> Option<i64> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.is_empty() {
            return None;
        }

        let first = (now - OTP_ALLOWED_DRIFT).div_euclid(OTP_INTERVAL);
        let last = (now + OTP_ALLOWED_DRIFT).div_euclid(OTP_INTERVAL);

        (first..=last)
            .filter(|step| consumed_timestep.is_none_or(|consumed| *step > consumed))
            .find(|step| bool::from(self.code_for_step(*step).as_bytes().ct_eq(code.as_bytes())))
    }

    /// `otp_provisioning_uri(email, issuer:)`, the QR code contents for authenticator apps.
    #[must_use]
    pub fn provisioning_uri(secret: &str, account: &str, issuer: &str) -> String {
        format!(
            "otpauth://totp/{}:{}?secret={secret}&issuer={}",
            uri_escape(issuer),
            uri_escape(account),
            uri_escape(issuer)
        )
    }
}

/// `ERB::Util.url_encode`, as ROTP uses for the label and the issuer.
fn uri_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(
                BASE32_ALPHABET[((buffer >> bits) & 31) as usize],
            ));
        }
    }
    if bits > 0 {
        out.push(char::from(
            BASE32_ALPHABET[((buffer << (5 - bits)) & 31) as usize],
        ));
    }
    out
}

/// ROTP's decoder: case-insensitive, ignores `=` padding.
fn base32_decode(secret: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in secret.trim_end_matches('=').bytes() {
        let value = BASE32_ALPHABET
            .iter()
            .position(|a| *a == c.to_ascii_uppercase())?;
        buffer = (buffer << 5) | u32::try_from(value).ok()?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_tokens_have_devise_shape() {
        let token = friendly_token();
        assert_eq!(token.len(), 20);
        assert!(!token.contains(['l', 'I', 'O', '0']));
    }

    #[test]
    fn base32_round_trips() {
        let secret = Totp::generate_secret();
        assert_eq!(secret.len(), 32);
        assert_eq!(
            base32_encode(&base32_decode(&secret).unwrap()),
            secret,
            "secret {secret}"
        );
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn verify_respects_drift_and_consumed_steps() {
        let totp = Totp::new("JBSWY3DPEHPK3PXP").unwrap();
        let now = 1_794_052_800;
        let step = now / 30;

        assert_eq!(totp.verify(&totp.at(now), now, None), Some(step));
        assert_eq!(totp.verify(&totp.at(now - 60), now, None), Some(step - 2));
        assert_eq!(totp.verify(&totp.at(now - 90), now, None), None);
        assert_eq!(totp.verify(&totp.at(now), now, Some(step)), None);
        let spaced = totp.at(now).chars().fold(String::new(), |mut s, c| {
            s.push(c);
            s.push(' ');
            s
        });
        assert_eq!(totp.verify(&spaced, now, Some(step - 1)), Some(step));
        assert_eq!(totp.verify("", now, None), None);
    }
}
