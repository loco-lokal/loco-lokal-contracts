//! Devise `database_authenticatable` passwords: bcrypt over `password + pepper`,
//! cost 10 (`config/initializers/devise.rb`), stored as `$2a$…`.

const COST: u32 = 10;

/// Checks a password against `users.encrypted_password`.
#[must_use]
pub fn verify(password: &str, encrypted_password: &str, pepper: Option<&str>) -> bool {
    if encrypted_password.is_empty() {
        return false;
    }

    let candidate = format!("{password}{}", pepper.unwrap_or_default());
    bcrypt::verify(candidate, encrypted_password).unwrap_or(false)
}

/// Hashes a password for `users.encrypted_password`.
pub fn hash(password: &str, pepper: Option<&str>) -> Result<String, bcrypt::BcryptError> {
    let candidate = format!("{password}{}", pepper.unwrap_or_default());
    Ok(bcrypt::hash_with_result(candidate, COST)?.format_for_version(bcrypt::Version::TwoA))
}
