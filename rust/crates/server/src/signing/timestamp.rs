//! `Submissions::TimestampHandler` and `Accounts.load_timeserver_url`: RFC 3161 timestamp
//! tokens for the signatures of result documents and audit trails.

use std::time::Duration;

use chrono::Utc;
use loco_db::signing;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::results::TIMESTAMP_SERVER_URL_KEY;
use crate::{errors::AppError, state::AppState};

/// `TimestampHandler::TIMEOUT`.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Faraday's default `User-Agent` (`faraday` 2.14.3 in `Gemfile.lock`).
const USER_AGENT: &str = "Faraday v2.14.3";

/// `TimestampHandler.new(tsa_url:)`.
pub struct TimestampHandler {
    client: reqwest::Client,
    url: String,
    fallback_url: Option<String>,
}

impl TimestampHandler {
    /// `Accounts.load_timeserver_url(account)`: the account's `timestamp_server_url`, else
    /// the first account's, when present.
    pub async fn load(state: &AppState, account_id: i64) -> Result<Option<Self>, AppError> {
        let url = signing::encrypted_config_with_fallback(
            &state.db,
            account_id,
            TIMESTAMP_SERVER_URL_KEY,
        )
        .await?
        .and_then(|v| state.secrets.encryptor.decrypt(&v).ok())
        .and_then(|v| serde_json::from_str::<Value>(&v).ok());
        let url = match url {
            Some(Value::String(url)) if !url.trim().is_empty() => url,
            _ => return Ok(None),
        };
        // `tsa_url.split(',')`: Ruby drops trailing empty fields.
        let mut parts = url.split(',');
        let first = parts.next().unwrap_or_default().to_owned();
        let fallback_url = parts.next().filter(|p| !p.is_empty()).map(str::to_owned);
        Ok(Some(Self {
            client: state.http.clone(),
            url: first,
            fallback_url,
        }))
    }

    /// `sign(io, byte_range)` over a signature value: the `TimeStampToken` DER from the
    /// timestamp server (or the fallback server when the first answers with an error or
    /// nothing). Any failure yields a `GeneralizedTime` of now instead, as in Rails.
    pub async fn sign(&self, signature: &[u8]) -> Vec<u8> {
        match self.request(&Sha256::digest(signature)).await {
            Ok(token) => token,
            Err(err) => {
                tracing::error!(error = %err, "timestamp server");
                generalized_time_now()
            }
        }
    }

    async fn request(&self, digest: &[u8]) -> anyhow::Result<Vec<u8>> {
        let payload = request_der(digest);
        // `Faraday.new(uri.origin)` posting to `uri.request_uri`, with basic auth only when
        // the URL has a password.
        let url = url::Url::parse(&self.url)?;
        let mut target = url.clone();
        let _ = target.set_username("");
        let _ = target.set_password(None);
        target.set_fragment(None);
        let mut request = self
            .client
            .post(target.as_str())
            .timeout(TIMEOUT)
            .header("content-type", "application/timestamp-query")
            .header("user-agent", USER_AGENT)
            .body(payload.clone());
        if let Some(password) = url.password().filter(|p| !p.is_empty()) {
            request = request.basic_auth(url.username(), Some(password));
        }
        let mut response = request.send().await?;
        let mut status = response.status();
        let mut body = response.bytes().await?;
        if status != reqwest::StatusCode::OK || body.is_empty() {
            let fallback = self
                .fallback_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("TimestampError: {status}"))?;
            response = self
                .client
                .post(fallback)
                .header("content-type", "application/timestamp-query")
                .header("user-agent", USER_AGENT)
                .body(payload)
                .send()
                .await?;
            status = response.status();
            body = response.bytes().await?;
            if status != reqwest::StatusCode::OK || body.is_empty() {
                anyhow::bail!("TimestampError: {status}");
            }
        }
        token(&body).ok_or_else(|| anyhow::anyhow!("no timestamp token in the response"))
    }
}

/// `OpenSSL::Timestamp::Request` with `algorithm = 'SHA256'` and the message imprint:
/// version 1, `certReq` true, no policy or nonce.
fn request_der(digest: &[u8]) -> Vec<u8> {
    const SHA256: &[u8] = &[
        0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00,
    ];
    let algorithm = der(0x30, SHA256);
    let imprint = der(0x30, &[algorithm, der(0x04, digest)].concat());
    der(
        0x30,
        &[&[0x02, 0x01, 0x01][..], &imprint, &[0x01, 0x01, 0xFF]].concat(),
    )
}

/// `OpenSSL::Timestamp::Response.new(body).token.to_der`: the `timeStampToken` of a
/// `TimeStampResp` (`SEQUENCE { status PKIStatusInfo, timeStampToken OPTIONAL }`).
fn token(body: &[u8]) -> Option<Vec<u8>> {
    let (tag, content, rest) = read(body)?;
    if tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (status_tag, _, after_status) = read(content)?;
    if status_tag != 0x30 || after_status.is_empty() {
        return None;
    }
    let (token_tag, _, after_token) = read(after_status)?;
    (token_tag == 0x30 && after_token.is_empty()).then(|| after_status.to_vec())
}

/// One DER element: its tag, its content and what follows it.
fn read(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)?;
    let (length, header) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 {
            return None;
        }
        let bytes = data.get(2..2 + count)?;
        let length = bytes
            .iter()
            .fold(0_usize, |acc, b| (acc << 8) | usize::from(*b));
        (length, 2 + count)
    };
    let content = data.get(header..header + length)?;
    Some((tag, content, &data[header + length..]))
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
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

/// `OpenSSL::ASN1::GeneralizedTime.new(Time.now.utc).to_der`.
fn generalized_time_now() -> Vec<u8> {
    der(
        0x18,
        Utc::now().format("%Y%m%d%H%M%SZ").to_string().as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_requests_like_openssl() {
        // `OpenSSL::Timestamp::Request` for SHA-256("abc").
        let expected = "30390201013031300d060960864801650304020105000420ba7816bf8f01cfea414140de\
                        5dae2223b00361a396177a9cb410ff61f20015ad0101ff";
        assert_eq!(hex::encode(request_der(&Sha256::digest(b"abc"))), expected);
    }

    #[test]
    fn reads_the_token_of_a_response() {
        let status = der(0x30, &[0x02, 0x01, 0x00]);
        let token_der = der(0x30, &[0x06, 0x01, 0x2A]);
        let response = der(0x30, &[status.clone(), token_der.clone()].concat());
        assert_eq!(token(&response), Some(token_der));
        assert_eq!(token(&der(0x30, &status)), None);
        assert_eq!(token(b"not der"), None);
    }

    #[test]
    fn generalized_time_is_utc() {
        let time = generalized_time_now();
        assert_eq!(time[..2], [0x18, 15]);
        assert_eq!(time.last(), Some(&b'Z'));
    }
}
