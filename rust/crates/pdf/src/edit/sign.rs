//! Detached CMS signatures (`adbe.pkcs7.detached`), built like
//! `HexaPDF::DigitalSignature::Signing::SignedDataCreator` for RSA keys and SHA-256.

use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use sha2::{Digest as _, Sha256};
use x509_parser::prelude::{FromDer as _, X509Certificate};

use super::EditError;

/// The certificate, its chain, and the key a document is signed with.
pub struct SigningKey {
    /// DER of the signing certificate.
    pub certificate: Vec<u8>,
    /// DER of the CA certificates (`pkcs.ca_certs`).
    pub chain: Vec<Vec<u8>>,
    pub key: RsaPrivateKey,
}

impl SigningKey {
    /// From PEM: the certificate, its PKCS#1 or PKCS#8 key, and the CA certificates.
    pub fn from_pem(certificate: &str, key: &str, chain: &[&str]) -> Result<Self, EditError> {
        use rsa::{pkcs1::DecodeRsaPrivateKey as _, pkcs8::DecodePrivateKey as _};
        let der = |pem: &str| {
            x509_parser::pem::parse_x509_pem(pem.as_bytes())
                .map(|(_, pem)| pem.contents)
                .map_err(|e| EditError::Sign(e.to_string()))
        };
        let key = RsaPrivateKey::from_pkcs1_pem(key)
            .or_else(|_| RsaPrivateKey::from_pkcs8_pem(key))
            .map_err(|e| EditError::Sign(e.to_string()))?;
        Ok(Self {
            certificate: der(certificate)?,
            chain: chain.iter().map(|pem| der(pem)).collect::<Result<_, _>>()?,
            key,
        })
    }
}

const OID_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01];
const OID_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
const OID_CONTENT_TYPE: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x03];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
const OID_SIGNING_TIME: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x05];
const OID_SIGNING_CERTIFICATE_V2: &[u8] = &[
    0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x2F,
];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(u8::try_from(len).unwrap_or_default());
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | u8::try_from(bytes.len()).unwrap_or_default());
        out.extend_from_slice(&bytes);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn set(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x31, &parts.concat())
}

fn oid(value: &[u8]) -> Vec<u8> {
    tlv(0x06, value)
}

fn null() -> Vec<u8> {
    vec![0x05, 0x00]
}

fn attribute(kind: &[u8], value: &[u8]) -> Vec<u8> {
    seq(&[&oid(kind), &set(&[value])])
}

/// Replaces the outer tag (implicit context-specific tagging, constructed).
fn implicit(tag: u8, mut der: Vec<u8>) -> Vec<u8> {
    der[0] = 0xA0 | tag;
    der
}

/// `SignedDataCreator.create(data, type: :cms)`.
pub fn signed_data(
    data: &[u8],
    key: &SigningKey,
    signing_time: &str,
) -> Result<Vec<u8>, EditError> {
    let (_, certificate) =
        X509Certificate::from_der(&key.certificate).map_err(|e| EditError::Sign(e.to_string()))?;
    let issuer = certificate.tbs_certificate.issuer.as_raw();
    let serial = tlv(0x02, certificate.tbs_certificate.raw_serial());
    let sha256 = seq(&[&oid(OID_SHA256), &null()]);

    let signing_certificate = seq(&[&seq(&[&seq(&[
        &tlv(0x04, &Sha256::digest(&key.certificate)),
        &seq(&[&seq(&[&tlv(0xA4, issuer)]), &serial]),
    ])])]);
    let attributes = [
        attribute(OID_CONTENT_TYPE, &oid(OID_DATA)),
        attribute(OID_SIGNING_TIME, &tlv(0x17, signing_time.as_bytes())),
        attribute(OID_MESSAGE_DIGEST, &tlv(0x04, &Sha256::digest(data))),
        attribute(OID_SIGNING_CERTIFICATE_V2, &signing_certificate),
    ];
    let parts: Vec<&[u8]> = attributes.iter().map(Vec::as_slice).collect();
    let signed_attrs = set(&parts);

    // `key.sign_raw('sha256', hash)`: PKCS#1 v1.5 over the DigestInfo.
    let mut digest_info = seq(&[&sha256, &tlv(0x04, &Sha256::digest(&signed_attrs))]);
    let signature = key
        .key
        .sign(
            Pkcs1v15Sign::new_unprefixed(),
            &std::mem::take(&mut digest_info),
        )
        .map_err(|e| EditError::Sign(e.to_string()))?;

    let signer_info = seq(&[
        &tlv(0x02, &[1]),
        &seq(&[issuer, &serial]),
        &sha256,
        &implicit(0, signed_attrs),
        &seq(&[&oid(OID_RSA), &null()]),
        &tlv(0x04, &signature),
    ]);
    let mut certificates = key.certificate.clone();
    for ca in &key.chain {
        certificates.extend_from_slice(ca);
    }
    let signed = seq(&[
        &tlv(0x02, &[1]),
        &set(&[&sha256]),
        &seq(&[&oid(OID_DATA)]),
        &tlv(0xA0, &certificates),
        &set(&[&signer_info]),
    ]);
    Ok(seq(&[&oid(OID_SIGNED_DATA), &tlv(0xA0, &signed)]))
}

/// `UTCTime` for a signing time given as `YYYYMMDDHHMMSS` (UTC).
#[must_use]
pub fn utc_time(timestamp: &str) -> String {
    format!("{}Z", timestamp.get(2..).unwrap_or(timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_long_lengths() {
        assert_eq!(tlv(0x04, &[0; 2])[..2], [0x04, 2]);
        let long = tlv(0x04, &[0; 300]);
        assert_eq!(long[..4], [0x04, 0x82, 0x01, 0x2C]);
    }
}
