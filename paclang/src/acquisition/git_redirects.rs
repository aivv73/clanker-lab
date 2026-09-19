//! Resolve refused off-site discovery redirects separately. libgit2 keeps
//! off-site redirects disabled, including those from upload-pack POSTs;
//! its existing same-host redirect behavior remains in effect.
use super::{Error, Limits, http::validate_url};
use curl::easy::Easy;
use std::collections::BTreeSet;
use std::time::Duration;
use url::Url;

fn repository_url(discovery: &Url) -> Result<String, Error> {
    if discovery.query() != Some("service=git-upload-pack") {
        return Err(Error::Failed("Git redirect changed the discovery service"));
    }
    let path = discovery
        .path()
        .strip_suffix("/info/refs")
        .filter(|path| !path.is_empty())
        .ok_or(Error::Failed(
            "Git redirect is not a repository discovery URL",
        ))?;
    let mut repo = discovery.clone();
    repo.set_path(path);
    repo.set_query(None);
    Ok(repo.to_string())
}

fn redirect_url(current: &Url, location: &str) -> Result<Url, Error> {
    if location.len() > 4096 || location.chars().any(char::is_control) {
        return Err(Error::Failed("invalid Git redirect Location"));
    }
    let next = current
        .join(location)
        .map_err(|_| Error::Failed("invalid Git redirect URL"))?;
    let next = validate_url(next.as_str())?;
    if current.scheme() == "https" && next.scheme() != "https" {
        return Err(Error::Failed("Git redirect attempted an HTTPS downgrade"));
    }
    repository_url(&next)?;
    Ok(next)
}

pub(super) fn resolve(
    location: &str,
    proxy: Option<&str>,
    limits: &Limits,
) -> Result<String, Error> {
    let mut current = validate_url(location)?;
    if current.query().is_some_and(|query| query != "signed") {
        return Err(Error::Failed(
            "Git redirect discovery does not support repository URL queries",
        ));
    }
    current.set_path(&format!(
        "{}/info/refs",
        current.path().trim_end_matches('/')
    ));
    current.set_query(Some("service=git-upload-pack"));
    let mut visited = BTreeSet::new();
    for hop in 0..=5 {
        if !visited.insert(current.to_string()) {
            return Err(Error::Failed("Git redirect cycle detected"));
        }
        let mut request = Easy::new();
        let config = |_| Error::Failed("cannot configure Git redirect discovery");
        request.url(current.as_str()).map_err(config)?;
        request.proxy(proxy.unwrap_or("")).map_err(config)?;
        request.follow_location(false).map_err(config)?;
        request.nobody(true).map_err(config)?;
        request
            .connect_timeout(Duration::from_secs(15))
            .map_err(config)?;
        request
            .timeout(Duration::from_secs(limits.source_timeout_seconds.min(60)))
            .map_err(config)?;
        request
            .useragent(concat!("paclang/", env!("CARGO_PKG_VERSION")))
            .map_err(config)?;
        let mut location = None;
        let mut header_bytes = 0usize;
        let mut invalid = false;
        let result = {
            let mut transfer = request.transfer();
            transfer
                .header_function(|header| {
                    header_bytes = header_bytes.saturating_add(header.len());
                    if header_bytes > 65536 {
                        return false;
                    }
                    if header.starts_with(b"HTTP/") {
                        location = None;
                    }
                    if let Some(colon) = header.iter().position(|byte| *byte == b':')
                        && let (name, value) = (&header[..colon], &header[colon + 1..])
                        && name.eq_ignore_ascii_case(b"location")
                    {
                        let Ok(value) = std::str::from_utf8(value) else {
                            invalid = true;
                            return false;
                        };
                        if location.is_some() {
                            invalid = true;
                            return false;
                        }
                        location = Some(value.trim().to_owned());
                    }
                    true
                })
                .map_err(config)?;
            transfer.perform()
        };
        if header_bytes > 65536 {
            return Err(Error::Limited("Git redirect header limit exceeded"));
        }
        if invalid {
            return Err(Error::Failed("invalid or duplicate Git redirect Location"));
        }
        result.map_err(|error| Error::Detail(format!("Git redirect discovery failed: {error}")))?;
        let status = request.response_code().map_err(config)?;
        match status {
            301 | 302 | 303 | 307 | 308 => {
                if hop == 5 {
                    return Err(Error::Failed("Git redirect limit exceeded"));
                }
                current = redirect_url(
                    &current,
                    location
                        .as_deref()
                        .ok_or(Error::Failed("Git redirect has no Location"))?,
                )?;
            }
            200 => return repository_url(&current),
            _ => {
                return Err(Error::Detail(format!(
                    "Git redirect discovery returned HTTP {status}"
                )));
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn discovery_rejects_missing_duplicate_and_oversized_headers() {
        for (headers, expected) in [
            (String::new(), "Git redirect has no Location"),
            ("Location: /a/info/refs?service=git-upload-pack\r\nLocation: /b/info/refs?service=git-upload-pack\r\n".to_owned(), "invalid or duplicate Git redirect Location"),
            (format!("X-Large: {}\r\n", "x".repeat(70000)), "Git redirect header limit exceeded"),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(request.starts_with(b"HEAD /repo.git/info/refs?service=git-upload-pack "));
                let _ = write!(stream, "HTTP/1.1 302 Found\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n");
            });
            let limits = Limits { source_jobs: 1, max_download_bytes: 1024, max_extracted_bytes: 1024, max_source_files: 10, source_timeout_seconds: 5 };
            let result = resolve(&url, None, &limits);
            server.join().unwrap();
            // Recent libcurl versions reject duplicate Location headers
            // before delivering the second one to our header callback.
            let rejected = match &result {
                Err(Error::Failed(reason) | Error::Limited(reason)) => *reason == expected,
                Err(Error::Detail(reason)) => expected == "invalid or duplicate Git redirect Location"
                    && reason.contains("Multiple Location headers"),
                _ => false,
            };
            assert!(rejected, "{expected}: {result:?}");
        }
    }

    #[test]
    fn git_redirects_preserve_https_and_discovery_shape() {
        let current = Url::parse(
            "https://git.savannah.gnu.org/git/coreutils.git/info/refs?service=git-upload-pack",
        )
        .unwrap();
        let next = redirect_url(&current, "https://https.git.savannah.gnu.org/git/coreutils.git/info/refs?service=git-upload-pack").unwrap();
        assert_eq!(
            repository_url(&next).unwrap(),
            "https://https.git.savannah.gnu.org/git/coreutils.git"
        );
        assert!(redirect_url(&current, "/new.git/info/refs?service=git-upload-pack").is_ok());
        for bad in [
            "http://127.0.0.1/repo/info/refs?service=git-upload-pack",
            "file:///tmp/repo",
            "https://user:secret@example.org/repo/info/refs?service=git-upload-pack",
            "/login",
            "/repo/info/refs?service=git-receive-pack",
            "/repo/info/refs?service=git-upload-pack#fragment",
            "/repo/info/refs?service=git-upload-pack&token=secret",
        ] {
            assert!(redirect_url(&current, bad).is_err(), "{bad}");
        }
    }
}
