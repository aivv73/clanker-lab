//! Explicit HTTP(S) proxy selection for networked refresh stages.
//!
//! Paclang never reads proxy environment variables implicitly: every
//! transport defaults to a direct connection. A proxy is used only when the
//! caller passes an explicit `--proxy-url`, which this module validates.
//! Proxy URLs carry no credentials, query, or fragment, so no secrets enter
//! the database or logs; TLS certificate and hostname verification still run
//! end to end, and a proxy learns request destinations but not content.

use url::Url;

/// A validated explicit proxy URL: `https`, or `http` only for literal
/// loopback addresses used by local fixtures. No credentials, query, or
/// fragment are accepted.
#[derive(Clone, Debug)]
pub(crate) struct ProxyUrl(Url);

impl std::str::FromStr for ProxyUrl {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_| "proxy URL must be an absolute URL")?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || value.chars().any(char::is_whitespace)
        {
            return Err(
                "proxy URL requires HTTPS without credentials, query, or fragment (HTTP is allowed only for literal loopback addresses)",
            );
        }
        Ok(Self(url))
    }
}

impl ProxyUrl {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::ProxyUrl;

    #[test]
    fn https_and_loopback_http_proxies_are_accepted() {
        for value in [
            "https://proxy.example.org:8080",
            "http://127.0.0.1:2080",
            "http://[::1]:3128/",
        ] {
            assert!(value.parse::<ProxyUrl>().is_ok(), "{value}");
        }
    }

    #[test]
    fn unsafe_proxy_urls_are_rejected() {
        for value in [
            "http://proxy.example.org:8080",
            "socks5://proxy.example.org:1080",
            "file:///tmp/proxy",
            "https://user:secret@example.org:8080",
            "https://proxy.example.org?target=x",
            "https://proxy.example.org#fragment",
            "https://proxy.example.org/white space",
            "",
        ] {
            assert!(value.parse::<ProxyUrl>().is_err(), "{value}");
        }
    }
}
