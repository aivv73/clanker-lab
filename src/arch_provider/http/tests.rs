use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use super::{FetchError, MAX_RESPONSE_BYTES, fetch, fetch_all, fetch_with_timeout};
use super::{ResponseBody, timeout_error};
use crate::proxy::ProxyUrl;
use curl::easy::Easy2;

fn server(response: Vec<u8>) -> (url::Url, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/recipe", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client never connected");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        // The bounded client may close early while a large response is sent.
        let _ = stream.write_all(&response);
    });
    (url, thread)
}

#[test]
fn successful_bodies_and_http_failures_are_distinct() {
    for (response, expected) in [
        (b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".to_vec(), Ok(b"abc".to_vec())),
        (b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(), Err(FetchError::Http(404))),
        (b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\nContent-Length: 0\r\n\r\n".to_vec(), Err(FetchError::Http(302))),
    ] {
        let (url, thread) = server(response);
        assert_eq!(fetch(&url), expected);
        thread.join().unwrap();
    }
}

#[test]
fn body_limit_applies_without_a_content_length() {
    let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
    response.extend(vec![b'x'; MAX_RESPONSE_BYTES + 1]);
    let (url, thread) = server(response);
    assert_eq!(
        fetch(&url),
        Err(FetchError::Failed("recipe API response exceeds 2 MiB"))
    );
    thread.join().unwrap();
}

#[test]
fn truncated_body_is_not_successful_metadata() {
    let (url, thread) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nabc".to_vec());
    let error = fetch(&url).unwrap_err();
    assert_eq!(error, FetchError::Curl(18));
    assert_eq!(
        error.to_string(),
        "recipe HTTP transfer failed (curl code 18)"
    );
    thread.join().unwrap();
}

#[test]
fn batch_fetches_keep_input_order_with_per_item_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let paths = ["/ok-one", "/missing", "/ok-two"];
    let thread = std::thread::spawn(move || {
        for _ in 0..paths.len() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "client never connected");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let body = if request.starts_with(b"GET /missing ") {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            } else if request.starts_with(b"GET /ok-one ") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\none".to_vec()
            } else {
                assert!(request.starts_with(b"GET /ok-two "));
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\ntwo".to_vec()
            };
            stream.write_all(&body).unwrap();
        }
    });
    let urls = paths
        .iter()
        .map(|path| format!("http://{address}{path}").parse().unwrap())
        .collect::<Vec<url::Url>>();
    let results = fetch_all(&urls, None).unwrap();
    assert_eq!(
        results,
        [
            Ok(b"one".to_vec()),
            Err(FetchError::Http(404)),
            Ok(b"two".to_vec()),
        ]
    );
    thread.join().unwrap();
}

#[test]
fn batch_fetch_uses_the_explicit_proxy() {
    let (origin_url, origin_thread) =
        server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec());
    // Minimal forwarding proxy for plain-HTTP loopback URLs: curl sends the
    // absolute URI, the proxy relays with an origin-form request line.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let proxy_thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client never connected");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            client.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let request_line = request
            .split(|byte| *byte == b'\r')
            .next()
            .expect("proxy request has a request line")
            .to_vec();
        // Absolute URI proves the client used this proxy.
        assert!(
            request_line.starts_with(b"GET http://127.0.0.1:"),
            "{}",
            String::from_utf8_lossy(&request_line)
        );
        let uri = request_line
            .strip_prefix(b"GET ")
            .and_then(|line| line.strip_suffix(b" HTTP/1.1"))
            .expect("proxy request line has absolute URI");
        let uri = String::from_utf8(uri.to_vec()).unwrap();
        let (authority, path) = uri
            .strip_prefix("http://")
            .and_then(|rest| rest.split_once('/'))
            .expect("absolute URI has authority and path");
        let mut upstream = std::net::TcpStream::connect(authority).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        upstream
            .write_all(format!("GET /{path} HTTP/1.1\r\n").as_bytes())
            .unwrap();
        let line_end = request
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|index| index + 2)
            .unwrap_or(request.len());
        upstream.write_all(&request[line_end..]).unwrap();
        std::io::copy(&mut upstream, &mut client).unwrap();
        request_line
    });
    let proxy: ProxyUrl = format!("http://{proxy_address}").parse().unwrap();
    let results = fetch_all(&[origin_url], Some(&proxy)).unwrap();
    assert_eq!(results, [Ok(b"hello".to_vec())]);
    proxy_thread.join().unwrap();
    origin_thread.join().unwrap();
}

#[test]
fn never_connected_handle_reports_connection_timeout() {
    // A handle that never transferred has no connect timestamp, so the
    // timeout phase resolves without any clock or network. The stalled
    // test below covers the complementary branch: its backlog-completed
    // TCP handshake yields a nonzero connect time and a request timeout.
    let easy = Easy2::new(ResponseBody::default());
    assert_eq!(
        timeout_error(&easy),
        FetchError::Failed("recipe connection timed out")
    );
}

#[test]
fn stalled_response_has_a_total_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/recipe", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let started = Instant::now();
    assert_eq!(
        fetch_with_timeout(&url, Duration::from_millis(100)),
        Err(FetchError::Failed("recipe request timed out"))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn retry_after_parses_seconds_dates_and_untrusted_values() {
    use std::time::UNIX_EPOCH;
    let now = UNIX_EPOCH + Duration::from_secs(784111777);
    assert_eq!(
        super::parse_retry_after(" 12\r\n", now),
        Some(Duration::from_secs(12))
    );
    assert_eq!(
        super::parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", now),
        Some(Duration::from_secs(10))
    );
    assert_eq!(
        super::parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
        Some(Duration::ZERO)
    );
    for value in ["", "-1", "+1", "garbage"] {
        assert_eq!(super::parse_retry_after(value, now), None);
    }
    assert_eq!(
        super::parse_retry_after("999999999999999999999999", now),
        Some(Duration::from_secs(u64::MAX))
    );
}

fn rate_limit_server(
    statuses: Vec<u16>,
    retry_after: &'static str,
) -> (url::Url, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/recipe", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let thread = std::thread::spawn(move || {
        for status in statuses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "retry never arrived");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            write!(stream, "HTTP/1.1 {status} Response\r\nRetry-After: {retry_after}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
        }
    });
    (url, thread)
}

#[test]
fn throttled_recipe_recovers_after_retry_after_delay() {
    let (url, thread) = rate_limit_server(vec![429, 200], "1");
    let start = Instant::now();
    assert_eq!(fetch_all(&[url], None).unwrap(), vec![Ok(b"ok".to_vec())]);
    assert!(start.elapsed() >= Duration::from_secs(1));
    thread.join().unwrap();
}

#[test]
fn persistent_throttling_stops_after_three_retries() {
    let (url, thread) = rate_limit_server(vec![429; 4], "0");
    let results = fetch_all(&[url], None).unwrap();
    assert!(matches!(results[0], Err(FetchError::RateLimited(_))));
    thread.join().unwrap();
}

#[test]
fn excessive_retry_after_is_not_retried_early() {
    let (url, thread) = rate_limit_server(vec![429], "999999999999999999999");
    let results = fetch_all(&[url], None).unwrap();
    assert!(matches!(results[0], Err(FetchError::RateLimited(_))));
    thread.join().unwrap();
}

#[test]
fn retry_preserves_successes_and_input_order() {
    let (ready, ready_thread) =
        server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nready".to_vec());
    let (throttled, retry_thread) = rate_limit_server(vec![429, 200], "0");
    assert_eq!(
        fetch_all(&[throttled, ready], None).unwrap(),
        vec![Ok(b"ok".to_vec()), Ok(b"ready".to_vec())]
    );
    ready_thread.join().unwrap();
    retry_thread.join().unwrap();
}
