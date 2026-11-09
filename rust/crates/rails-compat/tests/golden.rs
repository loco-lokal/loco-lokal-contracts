//! Checks against values produced by the Rails reference app
//! (`rust/scripts/generate-golden.sh`), using the dev-only `SECRET_KEY_BASE`.

use std::sync::LazyLock;

use chrono::{TimeZone, Utc};
use loco_rails_compat::{
    RailsSecrets, devise, message_verifier::VerifyError, password, signed_id_purpose,
};
use serde_json::{Value, json};

static GOLDEN: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("fixtures/golden.json")).expect("valid golden.json")
});

static SECRETS: LazyLock<RailsSecrets> =
    LazyLock::new(|| RailsSecrets::new(GOLDEN["secret_key_base"].as_str().unwrap(), None));

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn derives_the_same_encryption_key() {
    assert_eq!(
        hex::encode(SECRETS.encryptor.primary_key_bytes()),
        s(&GOLDEN["encryption"]["derived_primary_key_hex"])
    );
}

#[test]
fn decrypts_rails_encrypted_values() {
    for (name, case) in GOLDEN["encryption"]["values"].as_object().unwrap() {
        let decrypted = SECRETS.encryptor.decrypt(s(&case["ciphertext"]));
        assert_eq!(
            decrypted.as_deref(),
            Ok(s(&case["plaintext"])),
            "case {name}"
        );
    }
}

#[test]
fn encrypt_round_trips_and_compresses_long_values() {
    for case in GOLDEN["encryption"]["values"].as_object().unwrap().values() {
        let plaintext = s(&case["plaintext"]);
        let encrypted = SECRETS.encryptor.encrypt(plaintext);
        let headers = &serde_json::from_str::<Value>(&encrypted).unwrap()["h"];

        assert_eq!(
            SECRETS.encryptor.decrypt(&encrypted).as_deref(),
            Ok(plaintext)
        );
        assert_eq!(
            headers["c"].as_bool().unwrap_or(false),
            plaintext.len() > 140
        );
    }
}

#[test]
fn rejects_tampered_ciphertext() {
    let encrypted = s(&GOLDEN["encryption"]["values"]["short"]["ciphertext"]);
    let mut message: Value = serde_json::from_str(encrypted).unwrap();
    let payload = s(&message["p"]);
    let flipped = if payload.starts_with('A') { "B" } else { "A" };
    message["p"] = format!("{flipped}{}", &payload[1..]).into();

    assert!(SECRETS.encryptor.decrypt(encrypted).is_ok());
    assert!(SECRETS.encryptor.decrypt(&message.to_string()).is_err());
    assert!(SECRETS.encryptor.decrypt("plain text").is_err());
}

#[test]
fn verifies_rails_signed_ids() {
    let now = Utc.with_ymd_and_hms(2026, 11, 7, 0, 0, 0).unwrap();

    for (name, case) in GOLDEN["signed_id"]["values"].as_object().unwrap() {
        let result = SECRETS.signed_id_verifier.verify_at(
            s(&case["message"]),
            case["purpose"].as_str(),
            now,
        );

        if name == "expired" {
            assert_eq!(result, Err(VerifyError::Expired));
        } else {
            assert_eq!(result.as_ref(), Ok(&case["data"]), "case {name}");
        }
    }
}

#[test]
fn rejects_wrong_purpose_and_tampering() {
    let case = &GOLDEN["signed_id"]["values"]["attachment_uuid"];
    let message = s(&case["message"]);

    assert_eq!(
        SECRETS.signed_id_verifier.verify(message, Some("other")),
        Err(VerifyError::Purpose)
    );
    assert_eq!(
        SECRETS.signed_id_verifier.verify(message, None),
        Err(VerifyError::Purpose)
    );
    assert_eq!(
        SECRETS
            .signed_id_verifier
            .verify(&message.replacen('e', "f", 1), Some("attachment")),
        Err(VerifyError::Invalid)
    );
}

#[test]
fn generates_signed_ids_rails_would_accept() {
    let golden = &GOLDEN["signed_id"]["values"]["blob_proxy_no_purpose"];
    // Without purpose or expiry the output is deterministic and must match exactly.
    assert_eq!(
        SECRETS
            .signed_id_verifier
            .generate(&golden["data"], None, None),
        s(&golden["message"])
    );

    let purpose = signed_id_purpose("submitter", Some("download_completed"));
    let exp = Utc.with_ymd_and_hms(2126, 1, 1, 0, 0, 0).unwrap();
    let message = SECRETS
        .signed_id_verifier
        .generate(&json!(42), Some(&purpose), Some(exp));

    assert_eq!(
        SECRETS.signed_id_verifier.verify(&message, Some(&purpose)),
        Ok(json!(42))
    );
}

#[test]
fn active_storage_signed_blob_id_matches_exactly() {
    let golden = &GOLDEN["active_storage"]["blob_signed_id"];
    let verifier = &SECRETS.active_storage_verifier;

    assert_eq!(
        verifier.verify(s(&golden["message"]), Some("blob_id")),
        Ok(golden["data"].clone())
    );
    assert_eq!(
        verifier.generate(&golden["data"], Some("blob_id"), None),
        s(&golden["message"])
    );
}

#[test]
fn verifies_devise_bcrypt_hashes() {
    let bcrypt = &GOLDEN["bcrypt"];
    let hash = s(&bcrypt["hash"]);

    assert!(password::verify(s(&bcrypt["password"]), hash, None));
    assert!(!password::verify("wrong", hash, None));

    let ours = password::hash("new password", None).unwrap();
    assert!(ours.starts_with("$2a$10$"));
    assert!(password::verify("new password", &ours, None));
}

#[test]
fn digests_devise_reset_tokens() {
    let case = &GOLDEN["devise"]["reset_password_token"];
    let generator = devise::TokenGenerator::new(s(&GOLDEN["secret_key_base"]));
    assert_eq!(
        generator.digest("reset_password_token", s(&case["raw"])),
        s(&case["digest"])
    );
    assert_eq!(
        devise::friendly_token().len(),
        usize::try_from(GOLDEN["devise"]["friendly_token_length"].as_u64().unwrap()).unwrap()
    );
}

#[test]
fn matches_rotp_codes_and_provisioning_uri() {
    let case = &GOLDEN["devise"]["otp"];
    let totp = devise::Totp::new(s(&case["secret"])).unwrap();
    let now = case["unix_time"].as_i64().unwrap();

    assert_eq!(totp.at(now), s(&case["code"]));
    assert_eq!(totp.at(now - 30), s(&case["code_previous_step"]));
    assert_eq!(totp.verify(s(&case["code"]), now, None), Some(now / 30));
    assert_eq!(
        devise::Totp::provisioning_uri(
            s(&case["secret"]),
            "ana@example.test",
            s(&GOLDEN["devise"]["product_name"])
        ),
        s(&case["provisioning_uri"])
    );
}

#[test]
fn reads_and_writes_rails_encrypted_cookies() {
    let cookies = &GOLDEN["encrypted_cookies"];
    let encryptor = &SECRETS.cookie_encryptor;
    for name in ["start_form_slug", "email_2fa_slug"] {
        let golden = &cookies[name];
        assert_eq!(
            encryptor.decrypt(name, s(&golden["cookie"])),
            Some(golden["value"].clone())
        );
        // The purpose is bound to the cookie name.
        assert_eq!(encryptor.decrypt("other", s(&golden["cookie"])), None);
    }
    let exp = Utc.with_ymd_and_hms(2126, 1, 1, 0, 0, 0).unwrap();
    let written = encryptor.encrypt("start_form_slug", &json!("slug"), Some(exp));
    assert_eq!(
        encryptor.decrypt("start_form_slug", &written),
        Some(json!("slug"))
    );
    assert_eq!(encryptor.decrypt_at("start_form_slug", &written, exp), None);
}
