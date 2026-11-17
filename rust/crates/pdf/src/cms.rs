//! Reading and checking PKCS#7 `SignedData` as `OpenSSL::PKCS7` does for
//! `VerifyPdfSignature`: `PKCS7.new`, `#verify` of a detached signature (with a store
//! whose verify callback accepts every chain), `#signers`, `#certificates` and
//! `SignerInfo#signed_time`.
//!
//! Signatures with RSA keys and SHA-1 or SHA-2 digests are checked; other algorithms are
//! [`Unsupported`].

use rsa::{Pkcs1v15Sign, RsaPublicKey, pkcs8::DecodePublicKey as _};
use sha1::Sha1;
use sha2::{Digest as _, Sha224, Sha256, Sha384, Sha512};
use x509_parser::prelude::{FromDer as _, X509Certificate};

/// A part of a signature the Rust server cannot check yet.
#[derive(Debug, thiserror::Error)]
#[error("{0} is not supported yet")]
pub struct Unsupported(pub String);

/// A BER/DER element: tag byte, content, and the whole encoding.
#[derive(Clone, Copy, Debug)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    raw: &'a [u8],
}

/// Reads one element (definite or indefinite length) from the start of `data`.
fn read(data: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *data.first()?;
    if tag & 0x1F == 0x1F {
        return None;
    }
    let first = *data.get(1)?;
    let (header, length) = if first & 0x80 == 0 {
        (2, Some(usize::from(first)))
    } else if first == 0x80 {
        (2, None)
    } else {
        let n = usize::from(first & 0x7F);
        if n > 8 {
            return None;
        }
        let mut length = 0_usize;
        for byte in data.get(2..2 + n)? {
            length = length.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        (2 + n, Some(length))
    };
    let body = data.get(header..)?;
    if let Some(length) = length {
        let content = body.get(..length)?;
        let raw = &data[..header + length];
        return Some((Tlv { tag, content, raw }, &data[header + length..]));
    }
    // Indefinite length: elements up to the end-of-contents marker.
    if tag & 0x20 == 0 {
        return None;
    }
    let mut rest = body;
    loop {
        if rest.starts_with(&[0, 0]) {
            let used = body.len() - rest.len();
            let raw = &data[..header + used + 2];
            return Some((
                Tlv {
                    tag,
                    content: &body[..used],
                    raw,
                },
                &rest[2..],
            ));
        }
        let (_, next) = read(rest)?;
        rest = next;
    }
}

/// The elements inside a constructed element.
fn children(content: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        let (tlv, next) = read(rest)?;
        out.push(tlv);
        rest = next;
    }
    Some(out)
}

const OID_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
const OID_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
const OID_SIGNING_TIME: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x05];
const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01];

/// A signer of the `SignedData`.
#[derive(Clone, Debug)]
pub struct SignerInfo {
    /// DER of the issuer `Name`.
    pub issuer: Vec<u8>,
    /// The serial number's content bytes.
    pub serial: Vec<u8>,
    digest_algorithm: Vec<u8>,
    /// The `[0]` signed attributes, re-tagged as a `SET` for the signature.
    signed_attrs: Option<Vec<u8>>,
    message_digest: Option<Vec<u8>>,
    /// `signingTime` as `YYYYMMDDHHMMSS` (UTC).
    pub signing_time: Option<String>,
    signature_algorithm: Vec<u8>,
    signature: Vec<u8>,
}

/// A parsed `SignedData` (`OpenSSL::PKCS7.new`).
#[derive(Clone, Debug)]
pub struct Pkcs7 {
    /// The number of `digestAlgorithms`.
    pub digest_algorithms: usize,
    /// DER of the certificates.
    pub certificates: Vec<Vec<u8>>,
    pub signers: Vec<SignerInfo>,
}

fn oid_of(alg: &Tlv<'_>) -> Option<Vec<u8>> {
    let parts = children(alg.content)?;
    let oid = parts.first().filter(|t| t.tag == 0x06)?;
    Some(oid.content.to_vec())
}

/// `YYMMDDHHMMSSZ` (`UTCTime`) or `YYYYMMDDHHMMSSZ` (`GeneralizedTime`) as `YYYYMMDDHHMMSS`.
fn time_of(tlv: &Tlv<'_>) -> Option<String> {
    let text = std::str::from_utf8(tlv.content).ok()?;
    let digits = text.strip_suffix('Z')?;
    match (tlv.tag, digits.len()) {
        (0x17, 12) => {
            let year: u32 = digits[..2].parse().ok()?;
            let century = if year >= 50 { "19" } else { "20" };
            Some(format!("{century}{digits}"))
        }
        (0x18, 14) => Some(digits.to_owned()),
        _ => None,
    }
}

fn signer_info(tlv: &Tlv<'_>) -> Option<SignerInfo> {
    let parts = children(tlv.content)?;
    let mut iter = parts.iter();
    iter.next().filter(|t| t.tag == 0x02)?;
    // `IssuerAndSerialNumber`; PKCS#7 has no subject key identifier.
    let sid = iter.next().filter(|t| t.tag == 0x30)?;
    let sid_parts = children(sid.content)?;
    let issuer = sid_parts.first().filter(|t| t.tag == 0x30)?.raw.to_vec();
    let serial = sid_parts.get(1).filter(|t| t.tag == 0x02)?.content.to_vec();
    let digest_algorithm = oid_of(iter.next()?)?;
    let mut next = iter.next()?;
    let mut signed_attrs = None;
    let mut message_digest = None;
    let mut signing_time = None;
    if next.tag == 0xA0 {
        let mut set = next.raw.to_vec();
        set[0] = 0x31;
        for attribute in children(next.content)? {
            let fields = children(attribute.content)?;
            let (Some(oid), Some(values)) = (fields.first(), fields.get(1)) else {
                continue;
            };
            let value = children(values.content)?.first().copied();
            if oid.content == OID_MESSAGE_DIGEST {
                message_digest = value.filter(|v| v.tag == 0x04).map(|v| v.content.to_vec());
            } else if oid.content == OID_SIGNING_TIME {
                signing_time = value.as_ref().and_then(time_of);
            }
        }
        signed_attrs = Some(set);
        next = iter.next()?;
    }
    let signature_algorithm = oid_of(next)?;
    let signature = iter.next().filter(|t| t.tag == 0x04)?.content.to_vec();
    Some(SignerInfo {
        issuer,
        serial,
        digest_algorithm,
        signed_attrs,
        message_digest,
        signing_time,
        signature_algorithm,
        signature,
    })
}

/// The DER of a certificate's issuer, subject and serial, and its common name.
pub struct CertificateInfo {
    pub issuer: Vec<u8>,
    pub subject: Vec<u8>,
    pub serial: Vec<u8>,
    pub common_name: Option<String>,
    /// DER of the `SubjectPublicKeyInfo`.
    pub public_key: Vec<u8>,
}

/// The DER of a PEM certificate.
#[must_use]
pub fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .ok()
        .map(|(_, pem)| pem.contents)
}

/// Reads a certificate.
#[must_use]
pub fn certificate_info(der: &[u8]) -> Option<CertificateInfo> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let tbs = &cert.tbs_certificate;
    Some(CertificateInfo {
        issuer: tbs.issuer.as_raw().to_vec(),
        subject: tbs.subject.as_raw().to_vec(),
        serial: tbs.raw_serial().to_vec(),
        // `subject.to_a.assoc('CN')`: the first common name.
        common_name: tbs
            .subject
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .map(str::to_owned),
        public_key: tbs.subject_pki.raw.to_vec(),
    })
}

impl Pkcs7 {
    /// `OpenSSL::PKCS7.new(der)`; `None` where it raises.
    #[must_use]
    pub fn parse(der: &[u8]) -> Option<Self> {
        let (info, _) = read(der)?;
        let parts = children(info.content)?;
        if parts.first()?.content != OID_SIGNED_DATA {
            return None;
        }
        let explicit = parts.get(1).filter(|t| t.tag == 0xA0)?;
        let signed = *children(explicit.content)?.first()?;
        let fields = children(signed.content)?;
        let mut iter = fields.iter();
        iter.next().filter(|t| t.tag == 0x02)?;
        let algorithms = iter.next().filter(|t| t.tag == 0x31)?;
        let digest_algorithms = children(algorithms.content)?.len();
        iter.next().filter(|t| t.tag == 0x30)?;
        let mut next = iter.next()?;
        let mut certificates = Vec::new();
        if next.tag == 0xA0 {
            certificates = children(next.content)?
                .iter()
                .map(|c| c.raw.to_vec())
                .collect();
            next = iter.next()?;
        }
        if next.tag == 0xA1 {
            next = iter.next()?;
        }
        if next.tag != 0x31 {
            return None;
        }
        let signers = children(next.content)?
            .iter()
            .map(signer_info)
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            digest_algorithms,
            certificates,
            signers,
        })
    }

    /// `signer_certificate`: the certificate with the first signer's issuer and serial.
    #[must_use]
    pub fn signer_certificate(&self) -> Option<&[u8]> {
        let signer = self.signers.first()?;
        self.certificate_for(signer)
    }

    fn certificate_for(&self, signer: &SignerInfo) -> Option<&[u8]> {
        self.certificates
            .iter()
            .find(|der| {
                certificate_info(der)
                    .is_some_and(|c| c.issuer == signer.issuer && c.serial == signer.serial)
            })
            .map(Vec::as_slice)
    }

    /// `certificate_chain`: the common names from the signer's certificate up through its
    /// issuers among the included certificates.
    #[must_use]
    pub fn certificate_chain(&self) -> Vec<Option<String>> {
        let Some(signer) = self.signer_certificate().and_then(certificate_info) else {
            return Vec::new();
        };
        let all: Vec<CertificateInfo> = self
            .certificates
            .iter()
            .filter_map(|der| certificate_info(der))
            .collect();
        let mut chain = vec![signer];
        loop {
            let last = chain.last().map(|c| c.issuer.clone()).unwrap_or_default();
            let Some(issuer) = all.iter().find(|c| c.subject == last) else {
                break;
            };
            if chain
                .iter()
                .any(|c| c.subject == issuer.subject && c.serial == issuer.serial)
            {
                break;
            }
            chain.push(CertificateInfo {
                issuer: issuer.issuer.clone(),
                subject: issuer.subject.clone(),
                serial: issuer.serial.clone(),
                common_name: issuer.common_name.clone(),
                public_key: issuer.public_key.clone(),
            });
        }
        chain.into_iter().map(|c| c.common_name).collect()
    }

    /// `pkcs7.verify(certs, store, data, DETACHED | BINARY)` with a verify callback that
    /// accepts every chain: each signer's certificate is included, the content digest
    /// matches, and the signature checks.
    pub fn verify(&self, content: &[u8]) -> Result<bool, Unsupported> {
        if self.signers.is_empty() {
            return Ok(false);
        }
        for signer in &self.signers {
            let Some(cert) = self.certificate_for(signer).and_then(certificate_info) else {
                return Ok(false);
            };
            if !verify_signer(signer, &cert, content)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The digest of `data` with the algorithm and its `DigestInfo` algorithm identifier.
fn digest(oid: &[u8], data: &[u8]) -> Result<Vec<u8>, Unsupported> {
    let prefix = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02];
    Ok(match oid {
        [0x2B, 0x0E, 0x03, 0x02, 0x1A] => Sha1::digest(data).to_vec(),
        [p @ .., 0x01] if p == prefix => Sha256::digest(data).to_vec(),
        [p @ .., 0x02] if p == prefix => Sha384::digest(data).to_vec(),
        [p @ .., 0x03] if p == prefix => Sha512::digest(data).to_vec(),
        [p @ .., 0x04] if p == prefix => Sha224::digest(data).to_vec(),
        _ => return Err(Unsupported("This signature digest algorithm".into())),
    })
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(u8::try_from(len).unwrap_or(0));
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | u8::try_from(bytes.len()).unwrap_or(0));
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

fn verify_signer(
    signer: &SignerInfo,
    cert: &CertificateInfo,
    content: &[u8],
) -> Result<bool, Unsupported> {
    // RSASSA-PSS (`1.2.840.113549.1.1.10`) is not PKCS#1 v1.5.
    if !signer.signature_algorithm.starts_with(OID_RSA)
        || signer.signature_algorithm.as_slice() == [OID_RSA, &[0x01, 0x0A]].concat()
    {
        return Err(Unsupported("Verifying signatures without RSA keys".into()));
    }
    let Ok(key) = RsaPublicKey::from_public_key_der(&cert.public_key) else {
        return Err(Unsupported("Verifying signatures without RSA keys".into()));
    };
    let content_digest = digest(&signer.digest_algorithm, content)?;
    let digested = match &signer.signed_attrs {
        Some(attrs) => {
            if signer.message_digest.as_deref() != Some(content_digest.as_slice()) {
                return Ok(false);
            }
            digest(&signer.digest_algorithm, attrs)?
        }
        None => content_digest,
    };
    let oid = der(0x06, &signer.digest_algorithm);
    let hash = der(0x04, &digested);
    let with_null = der(
        0x30,
        &[
            der(0x30, &[oid.clone(), vec![0x05, 0x00]].concat()),
            hash.clone(),
        ]
        .concat(),
    );
    let without_null = der(0x30, &[der(0x30, &oid), hash].concat());
    Ok([with_null, without_null].iter().any(|info| {
        key.verify(Pkcs1v15Sign::new_unprefixed(), info, &signer.signature)
            .is_ok()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_indefinite_lengths() {
        let data = [0x30, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00, 0x04, 0x00];
        let (tlv, rest) = read(&data).expect("element");
        assert_eq!(tlv.tag, 0x30);
        assert_eq!(tlv.content, &[0x02, 0x01, 0x05]);
        assert_eq!(rest, &[0x04, 0x00]);
    }

    #[test]
    fn reads_utc_times() {
        let tlv = Tlv {
            tag: 0x17,
            content: b"261115100000Z",
            raw: b"",
        };
        assert_eq!(time_of(&tlv).as_deref(), Some("20261115100000"));
    }
}
