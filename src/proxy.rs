//! Resolve the externally visible base URL, honouring `X-Forwarded-*` only from trusted proxies.

use std::net::IpAddr;

use axum::http::{HeaderMap, header};

use crate::config::Config;

fn first(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Precedence: configured `base_url` > forwarded headers from a trusted peer > plain `Host`.
pub fn external_base_url(cfg: &Config, peer: IpAddr, headers: &HeaderMap) -> String {
    if let Some(b) = &cfg.base_url {
        return b.clone();
    }
    let host_hdr = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if cfg.is_trusted_proxy(peer) {
        let proto = first(headers, "x-forwarded-proto")
            .filter(|p| p == "http" || p == "https")
            .unwrap_or_else(|| "http".into());
        if let Some(host) = first(headers, "x-forwarded-host").or(host_hdr.clone()) {
            return format!("{proto}://{host}");
        }
    }
    format!(
        "http://{}",
        host_hdr.unwrap_or_else(|| cfg.listen.to_string())
    )
}

/// Client address: the socket peer, or the first `X-Forwarded-For` entry when the peer is a
/// trusted proxy.
pub fn client_ip(cfg: &Config, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if cfg.is_trusted_proxy(peer)
        && let Some(ip) = first(headers, "x-forwarded-for").and_then(|v| v.parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn cfg() -> Config {
        Config {
            trusted_proxies: vec!["172.18.0.0/16".parse().unwrap()],
            ..Config::default()
        }
    }

    #[test]
    fn trusted_peer_uses_forwarded() {
        let h = hm(&[
            ("host", "server:8080"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "cloud.example.com"),
        ]);
        assert_eq!(
            external_base_url(&cfg(), "172.18.0.2".parse().unwrap(), &h),
            "https://cloud.example.com"
        );
    }

    #[test]
    fn untrusted_peer_ignores_forwarded() {
        let h = hm(&[
            ("host", "evil:1"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "cloud.example.com"),
        ]);
        assert_eq!(
            external_base_url(&cfg(), "8.8.8.8".parse().unwrap(), &h),
            "http://evil:1"
        );
    }

    #[test]
    fn base_url_wins() {
        let c = Config {
            base_url: Some("https://fixed.example.com".into()),
            ..cfg()
        };
        let h = hm(&[("x-forwarded-host", "x")]);
        assert_eq!(
            external_base_url(&c, "172.18.0.2".parse().unwrap(), &h),
            "https://fixed.example.com"
        );
    }

    #[test]
    fn client_ip_only_from_trusted_peer() {
        let h = hm(&[("x-forwarded-for", "192.0.2.50, 172.18.0.2")]);
        assert_eq!(
            client_ip(&cfg(), "172.18.0.2".parse().unwrap(), &h).to_string(),
            "192.0.2.50"
        );
        assert_eq!(
            client_ip(&cfg(), "8.8.8.8".parse().unwrap(), &h).to_string(),
            "8.8.8.8"
        );
        let bad = hm(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(
            client_ip(&cfg(), "172.18.0.2".parse().unwrap(), &bad).to_string(),
            "172.18.0.2"
        );
    }

    #[test]
    fn bogus_proto_falls_back_to_http() {
        let h = hm(&[("host", "h"), ("x-forwarded-proto", "javascript")]);
        assert_eq!(
            external_base_url(&cfg(), "172.18.0.2".parse().unwrap(), &h),
            "http://h"
        );
    }
}
