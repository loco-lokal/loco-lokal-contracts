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

    /// `OpenSSL::PKCS12.new(der, password)`: the first private key, the first certificate
    /// whose public key matches it, and the other certificates in order as the chain
    /// (`ca_certs`), as OpenSSL's `PKCS12_parse` picks them.
    pub fn from_pkcs12(der: &[u8], password: &str) -> Result<Self, EditError> {
        use rsa::{pkcs8::DecodePrivateKey as _, traits::PublicKeyParts as _};
        const RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
        let archive = p12_keystore::Pkcs12Archive::from_pkcs12(der, password)
            .map_err(|e| EditError::Sign(format!("PKCS#12: {e}")))?;
        let bag = archive.keys.first().ok_or(EditError::Unsupported(
            "Signing with a certificate without a private key",
        ))?;
        if bag.key.oid().to_string() != RSA_ENCRYPTION {
            return Err(EditError::Unsupported("Signing with a non-RSA key"));
        }
        let key = RsaPrivateKey::from_pkcs8_der(bag.key.as_der())
            .map_err(|e| EditError::Sign(e.to_string()))?;
        let (modulus, exponent) = (key.n().to_bytes_be(), key.e().to_bytes_be());
        let matches = |der: &[u8]| {
            let Ok((_, certificate)) = X509Certificate::from_der(der) else {
                return false;
            };
            match certificate.public_key().parsed() {
                Ok(x509_parser::public_key::PublicKey::RSA(rsa)) => {
                    let trim = |b: &[u8]| -> Vec<u8> {
                        b.iter().copied().skip_while(|v| *v == 0).collect()
                    };
                    trim(rsa.modulus) == modulus && trim(rsa.exponent) == exponent
                }
                _ => false,
            }
        };
        let mut certificate = None;
        let mut chain = Vec::new();
        for bag in &archive.certs {
            let der = bag.cert.as_der().to_vec();
            if certificate.is_none() && matches(&der) {
                certificate = Some(der);
            } else {
                chain.push(der);
            }
        }
        let certificate = certificate.ok_or(EditError::Unsupported(
            "Signing with a private key without its certificate",
        ))?;
        Ok(Self {
            certificate,
            chain,
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
const OID_TIME_STAMP_TOKEN: &[u8] = &[
    0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x0E,
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

/// The timestamp token for a signature value (`timestamp_handler.sign`), embedded in the
/// signer info's unsigned attributes as `id-aa-timeStampToken`.
pub type Timestamper<'a> = &'a mut dyn FnMut(&[u8]) -> Vec<u8>;

/// `SignedDataCreator.create(data, type: :cms)`.
pub fn signed_data(
    data: &[u8],
    key: &SigningKey,
    signing_time: &str,
    timestamp: Option<Timestamper<'_>>,
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

    // `create_unsigned_attrs(signature)`.
    let unsigned_attrs = timestamp.map(|timestamp| {
        let token = timestamp(&signature);
        implicit(1, set(&[&attribute(OID_TIME_STAMP_TOKEN, &token)]))
    });
    let signer_info = seq(&[
        &tlv(0x02, &[1]),
        &seq(&[issuer, &serial]),
        &sha256,
        &implicit(0, signed_attrs),
        &seq(&[&oid(OID_RSA), &null()]),
        &tlv(0x04, &signature),
        unsigned_attrs.as_deref().unwrap_or_default(),
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
