//! Request helpers: the client IP (`request.remote_ip`) and redirects with Rails' status
//! codes.

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::ConnectInfo,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

/// `ActionDispatch::RemoteIp`: when the peer is a trusted proxy (loopback or private
/// network), the right-most untrusted address in `X-Forwarded-For`; otherwise the peer.
#[must_use]
pub fn client_ip(headers: &HeaderMap, peer: Option<&ConnectInfo<SocketAddr>>) -> Option<String> {
    let peer = peer.map(|ConnectInfo(addr)| addr.ip());

    if peer.is_none_or(trusted_proxy) {
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|ip| ip.trim().parse::<IpAddr>().ok())
            .collect::<Vec<_>>();

        if let Some(ip) = forwarded.iter().rev().find(|ip| !trusted_proxy(**ip)) {
            return Some(ip.to_string());
        }
        if let Some(ip) = forwarded.first() {
            return Some(ip.to_string());
        }
    }

    peer.map(|ip| ip.to_string())
}

fn trusted_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| trusted_proxy(IpAddr::V4(v4)))
        }
    }
}

fn redirect(status: StatusCode, location: &str) -> Response {
    match HeaderValue::from_str(location) {
        Ok(location) => (status, [(header::LOCATION, location)]).into_response(),
        Err(_) => (status, [(header::LOCATION, HeaderValue::from_static("/"))]).into_response(),
    }
}

/// `redirect_to` (302 Found).
#[must_use]
pub fn found(location: &str) -> Response {
    redirect(StatusCode::FOUND, location)
}

/// Devise responders' redirect (`config.responder.redirect_status = :see_other`).
#[must_use]
pub fn see_other(location: &str) -> Response {
    redirect(StatusCode::SEE_OTHER, location)
}

/// The client IP as an extractor.
pub struct ClientIp(pub Option<String>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    #[allow(clippy::unused_async_trait_impl)] // the trait requires an async fn
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(client_ip(
            &parts.headers,
            parts.extensions.get::<ConnectInfo<SocketAddr>>(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", value.parse().unwrap());
        headers
    }

    #[test]
    fn trusts_only_private_proxies() {
        let proxy = ConnectInfo("172.23.0.1:5000".parse::<SocketAddr>().unwrap());
        let public = ConnectInfo("203.0.113.9:5000".parse::<SocketAddr>().unwrap());

        assert_eq!(
            client_ip(&forwarded("198.51.100.1, 10.0.0.2"), Some(&proxy)).as_deref(),
            Some("198.51.100.1")
        );
        assert_eq!(
            client_ip(&forwarded("198.51.100.1"), Some(&public)).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            client_ip(&HeaderMap::new(), Some(&proxy)).as_deref(),
            Some("172.23.0.1")
        );
    }
}
