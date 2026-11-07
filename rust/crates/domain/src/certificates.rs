//! `GenerateCertificate`: the self-issued chain an account signs documents with when it
//! has no certificate of its own. It is a root CA, a sub-CA and a leaf certificate
//! (2048-bit RSA, SHA-256, valid for 100 years), stored as the `esign_certs` encrypted
//! config with the same PEM keys Rails writes (`cert`, `key`, `root_ca`, `root_key`,
//! `sub_ca`, `sub_key`). Keys are PKCS#1 (`BEGIN RSA PRIVATE KEY`), as Ruby's
//! `OpenSSL::PKey::RSA#to_pem` writes them.

use rand::RngExt as _;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose, PKCS_RSA_SHA256, SerialNumber,
};
use rsa::{
    RsaPrivateKey,
    pkcs1::EncodeRsaPrivateKey,
    pkcs8::{EncodePrivateKey, LineEnding},
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

const KEY_BITS: usize = 2048;
const VALIDITY_DAYS: i64 = 36_525;

#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error("generating an RSA key: {0}")]
    Rsa(#[from] rsa::Error),
    #[error("encoding an RSA key: {0}")]
    Encoding(String),
    #[error("building a certificate: {0}")]
    Rcgen(#[from] rcgen::Error),
}

/// The `esign_certs` value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EsignCerts {
    pub cert: String,
    pub key: String,
    pub root_ca: String,
    pub root_key: String,
    pub sub_ca: String,
    pub sub_key: String,
}

struct Key {
    pkcs1_pem: String,
    pair: KeyPair,
}

fn generate_key() -> Result<Key, CertificateError> {
    let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, KEY_BITS)?;
    let pkcs1_pem = key
        .to_pkcs1_pem(LineEnding::LF)
        .map_err(|e| CertificateError::Encoding(e.to_string()))?
        .to_string();
    let pkcs8_pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| CertificateError::Encoding(e.to_string()))?;
    let pair = KeyPair::from_pkcs8_pem_and_sign_algo(&pkcs8_pem, &PKCS_RSA_SHA256)?;
    Ok(Key { pkcs1_pem, pair })
}

fn params(common_name: &str, organization: &str, ca: bool) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CountryName, "AT");
    name.push(DnType::OrganizationName, organization);
    name.push(DnType::CommonName, common_name);
    params.distinguished_name = name;

    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(VALIDITY_DAYS);
    // OpenSSL::BN.rand(160): a random 160-bit serial with the top bit set.
    let mut serial: [u8; 20] = rand::rng().random();
    serial[0] |= 0x80;
    params.serial_number = Some(SerialNumber::from_slice(&serial));

    if ca {
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    } else {
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    }
    params
}

/// `GenerateCertificate.call(name)`; `name` is `Docuseal.product_name`.
pub fn generate(name: &str) -> Result<EsignCerts, CertificateError> {
    let root_key = generate_key()?;
    let sub_key = generate_key()?;
    let leaf_key = generate_key()?;

    let root = CertifiedIssuer::self_signed(
        params(&format!("{name} Root CA"), name, true),
        &root_key.pair,
    )?;
    let sub = CertifiedIssuer::signed_by(
        params(&format!("{name} Sub-CA"), name, true),
        &sub_key.pair,
        &root,
    )?;
    let leaf = params(name, name, false).signed_by(&leaf_key.pair, &sub)?;

    Ok(EsignCerts {
        cert: leaf.pem(),
        key: leaf_key.pkcs1_pem,
        root_ca: root.pem(),
        root_key: root_key.pkcs1_pem,
        sub_ca: sub.pem(),
        sub_key: sub_key.pkcs1_pem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_three_level_chain() {
        let certs = generate("DocuSeal").unwrap();

        for pem in [&certs.cert, &certs.root_ca, &certs.sub_ca] {
            assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        }
        for pem in [&certs.key, &certs.root_key, &certs.sub_key] {
            assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        }

        let parse = |pem: &str| {
            let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
            pem.contents
        };
        let leaf = parse(&certs.cert);
        let (_, leaf) = x509_parser::parse_x509_certificate(&leaf).unwrap();
        assert_eq!(leaf.subject().to_string(), "C=AT, O=DocuSeal, CN=DocuSeal");
        assert_eq!(
            leaf.issuer().to_string(),
            "C=AT, O=DocuSeal, CN=DocuSeal Sub-CA"
        );
        assert!(!leaf.is_ca());

        let sub = parse(&certs.sub_ca);
        let (_, sub) = x509_parser::parse_x509_certificate(&sub).unwrap();
        assert_eq!(
            sub.issuer().to_string(),
            "C=AT, O=DocuSeal, CN=DocuSeal Root CA"
        );
        assert!(sub.is_ca());
        leaf.verify_signature(Some(sub.public_key())).unwrap();
    }
}
