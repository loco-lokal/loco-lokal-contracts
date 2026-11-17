//! `OtpChallenges` (`lib/otp_challenges.rb`): six-digit HOTP codes (`ROTP::HOTP#at(0)`)
//! keyed by a value and a purpose, with a fresh nonce per challenge version.

use chrono::{Duration, Utc};
use hmac::{Hmac, KeyInit as _, Mac as _};
use loco_db::otp;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;

use crate::{errors::AppError, rate_limit::LimitApproached, state::AppState};

/// The purpose of the signing form and share link email codes.
pub const FORM_EMAIL_2FA: &str = "form_email_2fa";

const CODE_TTL: Duration = Duration::minutes(10);
const REUSE_MIN_TTL: Duration = Duration::minutes(5);
const MAX_ATTEMPTS: i64 = 20;
const MAX_TOTAL_ATTEMPTS: i64 = 100;
const ATTEMPTS_WINDOW: Duration = Duration::hours(23);

/// `build_key`: `SHA256("purpose:value")` in hex.
fn build_key(value: &str, purpose: &str) -> String {
    hex::encode(Sha256::digest(format!("{purpose}:{value}").as_bytes()))
}

/// `build_hotp(value, purpose, nonce).at(0)`. The HOTP secret is the Base32 of a SHA1
/// digest, which ROTP decodes back to the digest bytes.
fn code(secret_key_base: &str, value: &str, purpose: &str, nonce: &str) -> String {
    hotp(&Sha1::digest(
        format!("{secret_key_base}:{purpose}:{value}:{nonce}").as_bytes(),
    ))
}

/// `ROTP::HOTP#at(0)` (RFC 4226, six digits).
fn hotp(secret: &[u8]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("any key size");
    mac.update(&0_u64.to_be_bytes());
    let hmac = mac.finalize().into_bytes();
    let offset = usize::from(hmac[hmac.len() - 1] & 0x0f);
    let code = (u32::from(hmac[offset] & 0x7f) << 24)
        | (u32::from(hmac[offset + 1]) << 16)
        | (u32::from(hmac[offset + 2]) << 8)
        | u32::from(hmac[offset + 3]);
    format!("{:06}", code % 1_000_000)
}

/// `OtpChallenges.generate`: the code of the latest challenge if it is unverified and
/// valid for five more minutes, else of a new challenge version.
pub async fn generate(state: &AppState, value: &str, purpose: &str) -> Result<String, AppError> {
    let key = build_key(value, purpose);
    let now = Utc::now().naive_utc();
    let latest = otp::latest(&state.db, &key).await?;
    let nonce = match latest {
        Some(c) if c.verified_at.is_none() && c.expire_at > now + REUSE_MIN_TTL => c.nonce,
        _ => {
            let version = latest.map_or(1, |c| c.version + 1);
            otp::create_or_find(
                &state.db,
                &key,
                version,
                &loco_rails_compat::base58(16),
                now + CODE_TTL,
            )
            .await?
        }
    };
    Ok(code(&state.config.secret_key_base, value, purpose, &nonce))
}

/// `OtpChallenges.verify`: whether `code` verifies the latest live challenge (once). Too
/// many failed attempts raise `RateLimit::LimitApproached`.
pub async fn verify(
    state: &AppState,
    code_param: &str,
    value: &str,
    purpose: &str,
) -> Result<Result<bool, LimitApproached>, AppError> {
    let key = build_key(value, purpose);
    let now = Utc::now().naive_utc();
    let recent = otp::recent(&state.db, &key, now - ATTEMPTS_WINDOW).await?;
    let Some(latest) = recent.first() else {
        return Ok(Ok(false));
    };
    if latest.verified_at.is_some() || latest.expire_at < now {
        return Ok(Ok(false));
    }
    if recent.iter().map(|c| i64::from(c.attempts)).sum::<i64>() >= MAX_ATTEMPTS
        || otp::total_attempts(&state.db, &key).await? >= MAX_TOTAL_ATTEMPTS
    {
        return Ok(Err(LimitApproached));
    }
    let expected = code(&state.config.secret_key_base, value, purpose, &latest.nonce);
    // `ROTP::OTP#time_constant_compare`.
    let matches = !code_param.is_empty()
        && code_param.len() == expected.len()
        && code_param
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if matches {
        return Ok(Ok(otp::mark_verified(&state.db, latest.id).await?));
    }
    otp::increment_attempts(&state.db, latest.id).await?;
    Ok(Ok(false))
}

/// `code.to_s.gsub(/\D/, '')`.
#[must_use]
pub fn digits(code: &str) -> String {
    code.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_rails_keys() {
        // `Digest::SHA256.hexdigest('form_email_2fa:a@example.com:abc')`
        assert_eq!(
            build_key("a@example.com:abc", FORM_EMAIL_2FA),
            hex::encode(Sha256::digest(b"form_email_2fa:a@example.com:abc"))
        );
        assert_eq!(build_key("x", "p").len(), 64);
    }

    #[test]
    fn hotp_matches_rfc_4226() {
        // RFC 4226 appendix D, counter 0.
        assert_eq!(hotp(b"12345678901234567890"), "755224");
    }

    #[test]
    fn keeps_digits() {
        assert_eq!(digits(" 123-456 "), "123456");
    }
}
