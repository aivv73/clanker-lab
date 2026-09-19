#[cfg(test)]
mod tests;

use std::time::{Duration, Instant, SystemTime};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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

// Shared by recipe batches, provider fallbacks, and API requests in this process.
// Key by network origin (including port), never by provider name or URL path.
// None represents a Retry-After too large for Instant to represent.
static HOST_COOLDOWNS: OnceLock<Mutex<HashMap<String, Option<Instant>>>> = OnceLock::new();
const HOST_COOLDOWN: Duration = Duration::from_secs(60);
const HOST_DEFERRED: &str = "recipe request deferred: host cooldown after HTTP 429";
// Waiting out a finite cooldown costs one pause per throttling episode and
// lets later batches and provider fallbacks proceed. Skipping instead turned
// a single 429 into hundreds of lost requests (including the CachyOS Arch
// rebuild fallbacks, which share the GitLab origin with the Arch stage).
// GitLab backs pressure off with multi-minute Retry-After values (observed
// 455 s through a shared proxy), so the budget covers those; only absurdly
// long cooldowns still skip immediately.
const MAX_DEFER_WAIT: Duration = Duration::from_secs(600);

enum DeferState {
    Ready,
    Wait(Duration),
    Skip,
}

fn host_defer_state(url: &Url) -> DeferState {
    let mut hosts = HOST_COOLDOWNS.get_or_init(Default::default).lock().unwrap();
    let key = url.origin().ascii_serialization();
    match hosts.get(&key) {
        Some(None) => DeferState::Skip,
        Some(Some(until)) => {
            let now = Instant::now();
            if *until > now {
                let remaining = *until - now;
                if remaining > MAX_DEFER_WAIT {
                    DeferState::Skip
                } else {
                    DeferState::Wait(remaining)
                }
            } else {
                hosts.remove(&key);
                DeferState::Ready
            }
        }
        None => DeferState::Ready,
    }
}

fn host_is_deferred(url: &Url) -> bool {
    !matches!(host_defer_state(url), DeferState::Ready)
}

fn defer_host(url: &Url, retry_after: Option<Duration>) {
    let delay = retry_after.unwrap_or_default().max(HOST_COOLDOWN);
    let until = Instant::now().checked_add(delay);
    let mut hosts = HOST_COOLDOWNS.get_or_init(Default::default).lock().unwrap();
    hosts.entry(url.origin().ascii_serialization())
        .and_modify(|old| *old = match (*old, until) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        })
        .or_insert(until);
}

// The transport is driven synchronously; this state survives provider calls.
static HOST_PACING: OnceLock<Mutex<HashMap<String, HostPacing>>> = OnceLock::new();
const NORMAL_INTERVAL: Duration = Duration::from_millis(250);
const THROTTLED_INTERVAL: Duration = Duration::from_secs(1);

struct HostPacing {
    concurrency: usize,
    interval: Duration,
    next: Instant,
    successes: usize,
}

impl Default for HostPacing {
    fn default() -> Self {
        // Start conservative (one request at a time with a one-second pause)
        // and ramp up only after sustained success. Starting at the normal
        // limits caused the first throttled responses to defer nearly all
        // remaining work.
        Self { concurrency: 1, interval: THROTTLED_INTERVAL, next: Instant::now(), successes: 0 }
    }
}

fn observe_host(state: &mut HostPacing, throttled: bool, successes: usize) {
    if throttled {
        state.concurrency = 1;
        state.interval = THROTTLED_INTERVAL;
        state.successes = 0;
    } else if state.concurrency == 1 {
        state.successes += successes;
        if state.successes >= 8 {
            state.concurrency = 2;
            state.interval = NORMAL_INTERVAL;
            state.successes = 0;
        }
    }
    state.next = Instant::now() + state.interval;
}

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
                    Some((index, delay.unwrap_or_default().max(Duration::from_secs(1 << retry))))
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
        {
            let mut hosts = HOST_COOLDOWNS.get_or_init(Default::default).lock().unwrap();
            for (index, _) in &pending {
                hosts.remove(&chunk[*index].origin().ascii_serialization());
            }
        }
        let urls: Vec<_> = pending
            .iter()
            .map(|(index, _)| chunk[*index].clone())
            .collect();
        for ((index, _), result) in pending.into_iter().zip(fetch_chunk(&urls, config)?) {
            results[index] = result;
        }
    }
    // Exhausted retries open a host-wide circuit. Later batches and fallback
    // providers wait it out (up to MAX_DEFER_WAIT) instead of starting another
    // retry storm; only over-long cooldowns return an explicit deferred
    // result. Long Retry-After values are never shortened to our budget.
    for (url, result) in chunk.iter().zip(&results) {
        if let Err(FetchError::RateLimited(delay)) = result {
            defer_host(url, *delay);
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
    let mut results = vec![None; chunk.len()];
    while results.iter().any(Option::is_none) {
        if interrupt::is_requested() { return Err(FetchError::Interrupted); }
        let mut selected = Vec::new();
        let mut counts = HashMap::<String, usize>::new();
        let mut wait = Duration::from_secs(60);
        {
            let mut hosts = HOST_PACING.get_or_init(Default::default).lock().unwrap();
            for (index, url) in chunk.iter().enumerate() {
                if results[index].is_some() { continue; }
                match host_defer_state(url) {
                    // Finite cooldowns are waited out below: one pause per
                    // throttling episode, then later batches and provider
                    // fallbacks proceed. Only over-long cooldowns skip.
                    DeferState::Wait(remaining) => { wait = wait.min(remaining); continue; }
                    DeferState::Skip => {
                        results[index] = Some(Err(FetchError::Failed(HOST_DEFERRED)));
                        continue;
                    }
                    DeferState::Ready => {}
                }
                let key = url.origin().ascii_serialization();
                let state = hosts.entry(key.clone()).or_default();
                let remaining = state.next.saturating_duration_since(Instant::now());
                if !remaining.is_zero() { wait = wait.min(remaining); continue; }
                let count = counts.entry(key).or_default();
                if *count < state.concurrency && selected.len() < MAX_CONCURRENT_FETCHES {
                    selected.push(index);
                    *count += 1;
                }
            }
        }
        if selected.is_empty() {
            if results.iter().any(Option::is_none) { wait_for_retry(wait)?; }
            continue;
        }
        let urls: Vec<_> = selected.iter().map(|&i| chunk[i].clone()).collect();
        let fetched = fetch_wave(&urls, config)?;
        let mut observations = HashMap::<String, (bool, usize)>::new();
        for ((index, url), result) in selected.into_iter().zip(&urls).zip(fetched) {
            let entry = observations.entry(url.origin().ascii_serialization()).or_default();
            if matches!(&result, Err(FetchError::RateLimited(_))) {
                entry.0 = true;
            }
            // Never open the host-wide cooldown here: remaining items in this
            // chunk must still be paced and attempted, and scheduled retries
            // probe after their backoff. Only exhausted retries in
            // `fetch_chunk_with_retries` defer later batches. Deferring on the
            // first 429 skipped nearly all remaining work and reported a
            // completed refresh with almost nothing fetched.
            entry.1 += usize::from(result.is_ok());
            results[index] = Some(result);
        }
        let mut hosts = HOST_PACING.get_or_init(Default::default).lock().unwrap();
        for (key, (throttled, successes)) in observations {
            observe_host(hosts.entry(key).or_default(), throttled, successes);
        }
    }
    Ok(results.into_iter().map(Option::unwrap).collect())
}

fn fetch_wave(
    chunk: &[Url],
    config: &RequestConfig<'_>,
) -> Result<Vec<Result<Vec<u8>, FetchError>>, FetchError> {
    let multi = Multi::new();
    let mut results: Vec<Option<Result<Vec<u8>, FetchError>>> =
        (0..chunk.len()).map(|_| None).collect();
    let mut handles = Vec::new();
    for (index, url) in chunk.iter().enumerate() {
        if host_is_deferred(url) {
            results[index] = Some(Err(FetchError::Failed(HOST_DEFERRED)));
            continue;
        }
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
