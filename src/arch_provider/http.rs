#[cfg(test)]
mod tests;

use std::time::{Duration, Instant, SystemTime};

use curl::easy::{Easy2, Handler, WriteError};
use curl::multi::Multi;
use url::Url;

use crate::interrupt;
use crate::proxy::ProxyUrl;

/// Includes base64 expansion and JSON framing around a 1 MiB .SRCINFO.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum recipe requests in flight at once. Fetches are small metadata
/// documents, so this stays well below analysis-stage concurrency while
/// keeping a full-system refresh from degrading to one round trip per
/// package base.
pub(crate) const MAX_CONCURRENT_FETCHES: usize = 8;

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
    pub(crate) fn as_url(&self) -> &Url {
        &self.0
    }

    pub(crate) fn file_url(&self, project: &str, tag: &str) -> Url {
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
pub(crate) enum FetchError {
    Interrupted,
    Failed(&'static str),
    Curl(u32),
    Http(u32),
    RateLimited(Option<Duration>),
    /// A redirect response with its `Location` target. Only produced when
    /// the caller opts into redirect handling; the default path keeps
    /// reporting the bare status so recipe fetching never follows redirects.
    Redirect(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => f.write_str("recipe fetch interrupted"),
            Self::Failed(reason) => f.write_str(reason),
            Self::Curl(code) => write!(f, "recipe HTTP transfer failed (curl code {code})"),
            Self::RateLimited(_) => {
                f.write_str("recipe API returned HTTP 429 (retry limit reached)")
            }
            Self::Http(status) => write!(f, "recipe API returned HTTP {status}"),
            Self::Redirect(_) => f.write_str("request was redirected"),
        }
    }
}

#[derive(Default)]
struct ResponseBody {
    bytes: Vec<u8>,
    oversized: bool,
    retry_after: Option<Duration>,
    location: Option<String>,
    byte_limit: Option<usize>,
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

impl Handler for ResponseBody {
    fn header(&mut self, bytes: &[u8]) -> bool {
        if bytes.starts_with(b"HTTP/") {
            self.retry_after = None;
            self.location = None;
        } else if let Ok(header) = std::str::from_utf8(bytes)
            && let Some((name, value)) = header.split_once(':')
        {
            if name.eq_ignore_ascii_case("retry-after") {
                self.retry_after = self
                    .retry_after
                    .max(parse_retry_after(value, SystemTime::now()));
            } else if name.eq_ignore_ascii_case("location") {
                let value = value.trim();
                if !value.is_empty() && value.len() <= 4096 {
                    self.location = Some(value.to_owned());
                }
            }
        }
        true
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
        if interrupt::is_requested() {
            return Ok(0);
        }
        if bytes.len() > self.byte_limit.unwrap_or(MAX_RESPONSE_BYTES).saturating_sub(self.bytes.len()) {
            self.oversized = true;
            return Ok(0);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
}

/// Single-request entry point, kept for focused unit tests; the refresh
/// path uses [`fetch_all`]. No redirects, credentials, or source-URL
/// requests. Proxy environment variables are never read; only the explicit
/// proxy argument is used.
#[cfg(test)]
pub(crate) fn fetch(url: &Url) -> Result<Vec<u8>, FetchError> {
    fetch_with_timeout(url, REQUEST_TIMEOUT)
}

/// Fetch several recipe documents with at most [`MAX_CONCURRENT_FETCHES`]
/// requests in flight. Results keep the input order; each item carries its
/// own bytes or failure so one bad response never discards the rest. Only
/// interruption aborts the whole batch with `Err`. `None` connects directly.
pub(crate) fn fetch_all(
    urls: &[Url],
    proxy: Option<&ProxyUrl>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    fetch_all_with_timeout(urls, REQUEST_TIMEOUT, proxy)
}

/// Fetch several JSON documents from a source-code hosting API with the same
/// concurrency, timeout, and proxy rules as recipe fetching, plus an
/// optional bearer token and opt-in redirect handling. Used by the fast
/// language provider; recipe fetching never authenticates or redirects.
pub(crate) fn fetch_all_api(
    urls: &[Url],
    proxy: Option<&ProxyUrl>,
    auth: Option<&str>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    fetch_with_config(
        urls,
        &RequestConfig {
            timeout: REQUEST_TIMEOUT,
            proxy,
            auth,
            byte_limit: None,
            follow_redirects: true,
        },
    )
}

#[derive(Clone, Copy)]
struct RequestConfig<'a> {
    timeout: Duration,
    proxy: Option<&'a ProxyUrl>,
    auth: Option<&'a str>,
    byte_limit: Option<usize>,
    follow_redirects: bool,
}

fn fetch_all_with_timeout(
    urls: &[Url],
    timeout: Duration,
    proxy: Option<&ProxyUrl>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    fetch_all_limited(urls, timeout, proxy, None)
}

pub(crate) fn fetch_all_limited(
    urls: &[Url], timeout: Duration, proxy: Option<&ProxyUrl>, byte_limit: Option<usize>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    fetch_with_config(
        urls,
        &RequestConfig {
            timeout,
            proxy,
            auth: None,
            byte_limit,
            follow_redirects: false,
        },
    )
}

fn fetch_with_config(
    urls: &[Url],
    config: &RequestConfig<'_>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    if interrupt::is_requested() { return Err(FetchError::Interrupted); }
    let mut results = Vec::with_capacity(urls.len());
    for chunk in urls.chunks(MAX_CONCURRENT_FETCHES) {
        if interrupt::is_requested() {
            return Err(FetchError::Interrupted);
        }
        results.extend(fetch_chunk_with_retries(chunk, config)?);
    }
    Ok(results)
}

// Retry only throttled requests; successful results keep their original positions.
// A server asking for more than our wait budget is left unresolved, never retried early.
fn fetch_chunk_with_retries(
    chunk: &[Url],
    config: &RequestConfig<'_>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    let mut results = fetch_chunk(chunk, config)?;
    for retry in 0..3 {
        let pending: Vec<_> = results
            .iter()
            .enumerate()
            .filter_map(|(index, result)| match result {
                Err(FetchError::RateLimited(delay)) => {
                    Some((index, delay.unwrap_or(Duration::from_secs(1 << retry))))
                }
                _ => None,
            })
            .collect();
        let Some(delay) = pending.iter().map(|(_, delay)| *delay).max() else {
            break;
        };
        if delay > Duration::from_secs(60) {
            break;
        }
        wait_for_retry(delay)?;
        let urls: Vec<_> = pending
            .iter()
            .map(|(index, _)| chunk[*index].clone())
            .collect();
        for ((index, _), result) in pending.into_iter().zip(fetch_chunk(&urls, config)?) {
            results[index] = result;
        }
    }
    Ok(results)
}

fn wait_for_retry(delay: Duration) -> Result<(), FetchError> {
    let start = Instant::now();
    loop {
        if interrupt::is_requested() {
            return Err(FetchError::Interrupted);
        }
        let remaining = delay.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

/// Run one bounded chunk of requests on a single multi handle. Chunk-level
/// transport failures become per-item errors; only interruption propagates.
fn fetch_chunk(
    chunk: &[Url],
    config: &RequestConfig<'_>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    let multi = Multi::new();
    let mut results: Vec<Option<Result<Vec<u8>, FetchError>>> =
        (0..chunk.len()).map(|_| None).collect();
    let mut handles = Vec::new();
    for (index, url) in chunk.iter().enumerate() {
        match configure(url, config).and_then(|mut easy| {
            easy.get_mut().byte_limit = config.byte_limit;
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
    let mut transfers: Vec<Option<Result<(), curl::Error>>> =
        (0..chunk.len()).map(|_| None).collect();
    multi.messages(|message| {
        for (index, handle) in &handles {
            if let Some(completed) = message.result_for2(handle) {
                transfers[*index] = Some(completed);
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
        results[index] = Some(finish(&easy, transfers[index].take(), config.follow_redirects));
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
/// A timeout before TCP connects reports a connection timeout; a timeout
/// after connecting reports a request timeout. Redirect responses surface
/// their target only when the caller opted in; otherwise they stay plain
/// HTTP statuses so recipe fetching never follows redirects.
fn finish(
    easy: &Easy2<ResponseBody>,
    transfer: Option<Result<(), curl::Error>>,
    follow_redirects: bool,
) -> Result<Vec<u8>, FetchError> {
    if easy.get_ref().oversized {
        return Err(FetchError::Failed(if easy.get_ref().byte_limit.is_some() { "recipe snapshot exceeds download limit" } else { "recipe API response exceeds 2 MiB" }));
    }
    match transfer {
        Some(Ok(())) => {}
        Some(Err(error)) if error.is_operation_timedout() => return Err(timeout_error(easy)),
        Some(Err(error)) => return Err(FetchError::Curl(error.code())),
        None => {
            return Err(FetchError::Failed("recipe HTTP transfer did not complete"));
        }
    }
    let status = easy
        .response_code()
        .map_err(|_| FetchError::Failed("cannot configure recipe HTTP client"))?;
    if follow_redirects && matches!(status, 301 | 302 | 307 | 308)
        && let Some(location) = easy.get_ref().location.clone()
    {
        return Err(FetchError::Redirect(location));
    }
    if status == 429 {
        return Err(FetchError::RateLimited(easy.get_ref().retry_after));
    }
    if status != 200 {
        return Err(FetchError::Http(status));
    }
    Ok(easy.get_ref().bytes.clone())
}

fn timeout_error(easy: &Easy2<ResponseBody>) -> FetchError {
    let connected = easy.connect_time().is_ok_and(|time| !time.is_zero());
    if connected {
        FetchError::Failed("recipe request timed out")
    } else {
        FetchError::Failed("recipe connection timed out")
    }
}

fn configure(
    url: &Url,
    config: &RequestConfig<'_>,
) -> Result<Easy2<ResponseBody>, FetchError> {
    if !curl::Version::get().feature_async_dns() {
        return Err(FetchError::Failed(
            "recipe fetching requires libcurl asynchronous DNS",
        ));
    }
    let mut easy = Easy2::new(ResponseBody::default());
    let configuration_error = |_| FetchError::Failed("cannot configure recipe HTTP client");
    easy.url(url.as_str()).map_err(configuration_error)?;
    easy.proxy(config.proxy.map(ProxyUrl::as_str).unwrap_or(""))
        .map_err(configuration_error)?;
    easy.follow_location(false).map_err(configuration_error)?;
    easy.connect_timeout(Duration::from_secs(5))
        .map_err(configuration_error)?;
    easy.timeout(config.timeout).map_err(configuration_error)?;
    easy.useragent(concat!("paclang/", env!("CARGO_PKG_VERSION")))
        .map_err(configuration_error)?;
    if let Some(token) = config.auth {
        let mut headers = curl::easy::List::new();
        headers
            .append(&format!("Authorization: Bearer {token}"))
            .map_err(configuration_error)?;
        easy.http_headers(headers).map_err(configuration_error)?;
    }
    Ok(easy)
}

#[cfg(test)]
fn fetch_with_timeout(url: &Url, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    if interrupt::is_requested() {
        return Err(FetchError::Interrupted);
    }
    let easy = configure(
        url,
        &RequestConfig {
            timeout,
            proxy: None,
            auth: None,
            byte_limit: None,
            follow_redirects: false,
        },
    )?;
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
    let mut result: Option<Result<(), curl::Error>> = None;
    multi.messages(|message| {
        if let Some(completed) = message.result_for2(&handle) {
            result = Some(completed);
        }
    });
    let easy = multi
        .remove2(handle)
        .map_err(|_| FetchError::Failed("cannot finish recipe HTTP request"))?;
    finish(&easy, result, false)
}
