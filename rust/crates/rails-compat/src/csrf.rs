//! Authenticity tokens in the style of Rails' `form_authenticity_token`.
//!
//! Each session holds a random 32-byte secret. Pages embed a *masked* token
//! (`base64url(one_time_pad || (pad XOR secret))`), so the value differs on every render
//! (BREACH mitigation) but every masked token for a session verifies. The Vue and Turbo code
//! reads it from `<meta name="csrf-token">` and sends it as `X-CSRF-Token` or the
//! `authenticity_token` form field.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use subtle::ConstantTimeEq;

pub const TOKEN_LENGTH: usize = 32;

/// A new per-session CSRF secret, stored (base64url-encoded) in the session.
#[must_use]
pub fn generate_secret() -> [u8; TOKEN_LENGTH] {
    rand::random()
}

/// A masked token for embedding in a page.
#[must_use]
pub fn masked_token(secret: &[u8; TOKEN_LENGTH]) -> String {
    let pad: [u8; TOKEN_LENGTH] = rand::random();
    let mut out = Vec::with_capacity(TOKEN_LENGTH * 2);
    out.extend_from_slice(&pad);
    out.extend(pad.iter().zip(secret).map(|(p, s)| p ^ s));

    URL_SAFE_NO_PAD.encode(out)
}

/// Whether a submitted token was produced by [`masked_token`] for this secret.
#[must_use]
pub fn verify(secret: &[u8; TOKEN_LENGTH], submitted: &str) -> bool {
    let Ok(raw) = URL_SAFE_NO_PAD.decode(submitted.trim_end_matches('=')) else {
        return false;
    };

    if raw.len() != TOKEN_LENGTH * 2 {
        return false;
    }

    let (pad, masked) = raw.split_at(TOKEN_LENGTH);
    let unmasked: Vec<u8> = pad.iter().zip(masked).map(|(p, m)| p ^ m).collect();

    bool::from(unmasked.ct_eq(secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_tokens_differ_but_verify() {
        let secret = generate_secret();
        let a = masked_token(&secret);
        let b = masked_token(&secret);

        assert_ne!(a, b);
        assert!(verify(&secret, &a));
        assert!(verify(&secret, &b));
        assert!(!verify(&generate_secret(), &a));
        assert!(!verify(&secret, "garbage"));
    }
}
