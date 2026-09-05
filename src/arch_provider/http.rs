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

/// Maximum recipe requests in flight at once. Fetches are small metadata
/// documents, so this stays well below analysis-stage concurrency while
/// keeping a full-system refresh from degrading to one round trip per
/// package base.
pub(super) const MAX_CONCURRENT_FETCHES: usize = 8;

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

/// Single-request entry point, kept for focused unit tests; the refresh
/// path uses [`fetch_all`]. No redirects, credentials, proxy environment,
/// or source-URL requests.
#[cfg(test)]
pub(super) fn fetch(url: &Url) -> Result<Vec<u8>, FetchError> {
    fetch_with_timeout(url, REQUEST_TIMEOUT)
}

/// Fetch several recipe documents with at most [`MAX_CONCURRENT_FETCHES`]
/// requests in flight. Results keep the input order; each item carries its
/// own bytes or failure so one bad response never discards the rest. Only
/// interruption aborts the whole batch with `Err`.
pub(super) fn fetch_all(urls: &[Url]) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    fetch_all_with_timeout(urls, REQUEST_TIMEOUT)
}

fn fetch_all_with_timeout(
    urls: &[Url],
    timeout: Duration,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    if interrupt::is_requested() {
        return Err(FetchError::Interrupted);
    }
    let mut results = Vec::with_capacity(urls.len());
    for chunk in urls.chunks(MAX_CONCURRENT_FETCHES) {
        if interrupt::is_requested() {
            return Err(FetchError::Interrupted);
        }
        results.extend(fetch_chunk(chunk, timeout)?);
    }
    Ok(results)
}

/// Run one bounded chunk of requests on a single multi handle. Chunk-level
/// transport failures become per-item errors; only interruption propagates.
fn fetch_chunk(
    chunk: &[Url],
    timeout: Duration,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    let multi = Multi::new();
    let mut results: Vec<Option<Result<Vec<u8>, FetchError>>> =
        (0..chunk.len()).map(|_| None).collect();
    let mut handles = Vec::new();
    for (index, url) in chunk.iter().enumerate() {
        match configure(url, timeout).and_then(|easy| {
            multi
                .add2(easy)
                .map_err(|_| FetchError::Failed("cannot start recipe HTTP request"))
        }) {
            Ok(handle) => handles.push((index, handle)),
            Err(error) => results[index] = Some(Err(error)),
        }
    }
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
    let mut transfers: Vec<Option<Result<(), FetchError>>> =
        (0..chunk.len()).map(|_| None).collect();
    multi.messages(|message| {
        for (index, handle) in &handles {
            if let Some(completed) = message.result_for2(handle) {
                transfers[*index] = Some(completed.map_err(|error| {
                    if error.is_operation_timedout() {
                        FetchError::Failed("recipe request timed out")
                    } else {
                        FetchError::Failed("recipe HTTP transfer failed")
                    }
                }));
            }
        }
    });
    for (index, handle) in handles {
        let easy = multi
            .remove2(handle)
            .map_err(|_| FetchError::Failed("cannot finish recipe HTTP request"))?;
        if results[index].is_some() {
            continue;
        }
        results[index] = Some(finish(&easy, transfers[index].take()));
    }
    Ok(results
        .into_iter()
        .map(|result| {
            result.unwrap_or(Err(FetchError::Failed(
                "recipe HTTP transfer did not complete",
            )))
        })
        .collect())
}

/// Check one completed transfer: size cap, transport result, then status.
fn finish(
    easy: &Easy2<ResponseBody>,
    transfer: Option<Result<(), FetchError>>,
) -> Result<Vec<u8>, FetchError> {
    if easy.get_ref().oversized {
        return Err(FetchError::Failed("recipe API response exceeds 2 MiB"));
    }
    transfer.unwrap_or(Err(FetchError::Failed(
        "recipe HTTP transfer did not complete",
    )))?;
    let status = easy
        .response_code()
        .map_err(|_| FetchError::Failed("cannot configure recipe HTTP client"))?;
    if status != 200 {
        return Err(FetchError::Http(status));
    }
    Ok(easy.get_ref().bytes.clone())
}

fn configure(url: &Url, timeout: Duration) -> Result<Easy2<ResponseBody>, FetchError> {
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
    Ok(easy)
}

#[cfg(test)]
fn fetch_with_timeout(url: &Url, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    if interrupt::is_requested() {
        return Err(FetchError::Interrupted);
    }
    let easy = configure(url, timeout)?;
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
    finish(&easy, Some(result))
}
