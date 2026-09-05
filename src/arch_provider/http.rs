#[cfg(test)]
mod tests;

use std::time::Duration;

use curl::easy::{Easy2, Handler, WriteError};
use curl::multi::Multi;
use url::Url;

use crate::interrupt;

/// Includes base64 expansion and JSON framing around a 1 MiB .SRCINFO.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub(crate) struct RecipeEndpoint(Url);

impl std::str::FromStr for RecipeEndpoint {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_| "recipe API must be an absolute URL")?;
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
        {
            return Err(
                "recipe API requires HTTPS without credentials, query, or fragment (HTTP is allowed only for literal loopback addresses)",
            );
        }
        Ok(Self(url))
    }
}

impl RecipeEndpoint {
    pub(super) fn file_url(&self, project: &str, tag: &str) -> Url {
        let mut url = self.0.clone();
        // Constructor permits only hierarchical HTTP(S) URLs.
        url.path_segments_mut()
            .expect("HTTP URL has path segments")
            .pop_if_empty()
            .extend(["projects", project, "repository", "files", ".SRCINFO"]);
        url.query_pairs_mut().append_pair("ref", tag);
        url
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FetchError {
    Interrupted,
    Failed(&'static str),
    Http(u32),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => f.write_str("recipe fetch interrupted"),
            Self::Failed(reason) => f.write_str(reason),
            Self::Http(status) => write!(f, "recipe API returned HTTP {status}"),
        }
    }
}

#[derive(Default)]
struct ResponseBody {
    bytes: Vec<u8>,
    oversized: bool,
}

impl Handler for ResponseBody {
    fn write(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
        if interrupt::is_requested() {
            return Ok(0);
        }
        if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(self.bytes.len()) {
            self.oversized = true;
            return Ok(0);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
}

/// One request at a time; multi permits polling cancellation while the server
/// is idle. No redirects, credentials, proxy environment, or source-URL requests.
pub(super) fn fetch(url: &Url) -> Result<Vec<u8>, FetchError> {
    fetch_with_timeout(url, REQUEST_TIMEOUT)
}

fn fetch_with_timeout(url: &Url, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    if interrupt::is_requested() {
        return Err(FetchError::Interrupted);
    }
    if !curl::Version::get().feature_async_dns() {
        return Err(FetchError::Failed(
            "recipe fetching requires libcurl asynchronous DNS",
        ));
    }
    let mut easy = Easy2::new(ResponseBody::default());
    let configuration_error = |_| FetchError::Failed("cannot configure recipe HTTP client");
    easy.url(url.as_str()).map_err(configuration_error)?;
    easy.proxy("").map_err(configuration_error)?;
    easy.follow_location(false).map_err(configuration_error)?;
    easy.connect_timeout(Duration::from_secs(5))
        .map_err(configuration_error)?;
    easy.timeout(timeout).map_err(configuration_error)?;
    easy.useragent(concat!("paclang/", env!("CARGO_PKG_VERSION")))
        .map_err(configuration_error)?;
    let multi = Multi::new();
    let handle = multi
        .add2(easy)
        .map_err(|_| FetchError::Failed("cannot start recipe HTTP request"))?;
    loop {
        if interrupt::is_requested() {
            return Err(FetchError::Interrupted);
        }
        let running = multi
            .perform()
            .map_err(|_| FetchError::Failed("recipe HTTP transfer failed"))?;
        if running == 0 {
            break;
        }
        multi
            .wait(&mut [], Duration::from_millis(50))
            .map_err(|_| FetchError::Failed("recipe HTTP wait failed"))?;
    }
    if interrupt::is_requested() {
        return Err(FetchError::Interrupted);
    }
    let mut result = Err(FetchError::Failed("recipe HTTP transfer did not complete"));
    multi.messages(|message| {
        if let Some(completed) = message.result_for2(&handle) {
            result = completed.map_err(|error| {
                if error.is_operation_timedout() {
                    FetchError::Failed("recipe request timed out")
                } else {
                    FetchError::Failed("recipe HTTP transfer failed")
                }
            });
        }
    });
    let easy = multi
        .remove2(handle)
        .map_err(|_| FetchError::Failed("cannot finish recipe HTTP request"))?;
    if easy.get_ref().oversized {
        return Err(FetchError::Failed("recipe API response exceeds 2 MiB"));
    }
    result?;
    let status = easy.response_code().map_err(configuration_error)?;
    if status != 200 {
        return Err(FetchError::Http(status));
    }
    Ok(easy.get_ref().bytes.clone())
}
