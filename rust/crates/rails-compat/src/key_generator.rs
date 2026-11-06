//! `ActiveSupport::KeyGenerator`: PBKDF2-HMAC over a secret with a salt.

use sha1::Sha1;
use sha2::Sha256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Digest {
    Sha1,
    Sha256,
}

#[derive(Clone)]
pub struct KeyGenerator {
    secret: Vec<u8>,
    iterations: u32,
    digest: Digest,
}

impl KeyGenerator {
    #[must_use]
    pub fn new(secret: &[u8], iterations: u32, digest: Digest) -> Self {
        Self {
            secret: secret.to_vec(),
            iterations,
            digest,
        }
    }

    #[must_use]
    pub fn generate_key(&self, salt: &[u8], length: usize) -> Vec<u8> {
        let mut key = vec![0; length];
        match self.digest {
            Digest::Sha1 => {
                pbkdf2::pbkdf2_hmac::<Sha1>(&self.secret, salt, self.iterations, &mut key);
            }
            Digest::Sha256 => {
                pbkdf2::pbkdf2_hmac::<Sha256>(&self.secret, salt, self.iterations, &mut key);
            }
        }
        key
    }
}
