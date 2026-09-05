use super::{AcquisitionRecord, Error, Limits};
use curl::easy::{Easy, List};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use url::Url;

pub(super) fn validate_url(value: &str) -> Result<Url, Error> {
    let url = Url::parse(value).map_err(|_| Error::Failed("invalid source URL"))?;
    let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || value.chars().any(char::is_whitespace)
    {
        return Err(Error::Failed(
            "source URL requires HTTPS without credentials or fragment (HTTP only on literal loopback)",
        ));
    }
    Ok(url)
}

pub(super) fn download(
    location: &str,
    destination: &Path,
    limits: &Limits,
    record: &mut AcquisitionRecord,
) -> Result<(), Error> {
    if !curl::Version::get().feature_async_dns() {
        return Err(Error::Failed(
            "source downloading requires asynchronous DNS",
        ));
    }
    let mut url = validate_url(location)?;
    for _ in 0..=5 {
        let mut file = File::create(destination)?;
        let mut easy = Easy::new();
        let config = |_| Error::Failed("cannot configure source HTTP request");
        easy.url(url.as_str()).map_err(config)?;
        easy.proxy("").map_err(config)?;
        easy.follow_location(false).map_err(config)?;
        easy.connect_timeout(Duration::from_secs(15))
            .map_err(config)?;
        easy.timeout(Duration::from_secs(limits.source_timeout_seconds))
            .map_err(config)?;
        easy.useragent(concat!("paclang/", env!("CARGO_PKG_VERSION")))
            .map_err(config)?;
        // Checksums cover the archive bytes, not HTTP content decoding.
        let mut headers = List::new();
        headers
            .append("Accept-Encoding: identity")
            .map_err(config)?;
        easy.http_headers(headers).map_err(config)?;
        let mut limited = false;
        let mut io_failed = false;
        let result = {
            let mut transfer = easy.transfer();
            transfer
                .write_function(|bytes| {
                    if bytes.len() as u64
                        > limits
                            .max_download_bytes
                            .saturating_sub(record.downloaded_bytes)
                    {
                        limited = true;
                        return Ok(0);
                    }
                    if file.write_all(bytes).is_err() {
                        io_failed = true;
                        return Ok(0);
                    }
                    record.downloaded_bytes += bytes.len() as u64;
                    Ok(bytes.len())
                })
                .map_err(config)?;
            transfer.perform()
        };
        if limited {
            return Err(Error::Limited("source download byte limit exceeded"));
        }
        if io_failed {
            return Err(Error::Failed("cannot write source download"));
        }
        result.map_err(|error| {
            if error.is_operation_timedout() {
                Error::Limited("source deadline exceeded")
            } else {
                Error::Failed("source HTTP transfer failed")
            }
        })?;
        let status = easy.response_code().map_err(config)?;
        if status == 200 {
            file.sync_all()?;
            return Ok(());
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let redirect = easy
                .redirect_url()
                .map_err(config)?
                .ok_or(Error::Failed("source redirect has no location"))?;
            let next = validate_url(
                url.join(redirect)
                    .map_err(|_| Error::Failed("invalid source redirect"))?
                    .as_str(),
            )?;
            if url.scheme() == "https" && next.scheme() != "https" {
                return Err(Error::Failed("source redirect downgrades HTTPS"));
            }
            url = next;
        } else {
            return Err(Error::Detail(format!(
                "source server returned HTTP {status}"
            )));
        }
    }
    Err(Error::Failed("too many source redirects"))
}
