use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use super::{FetchError, MAX_RESPONSE_BYTES, fetch, fetch_all, fetch_with_timeout};

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
    assert!(matches!(fetch(&url), Err(FetchError::Failed(_))));
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
    let results = fetch_all(&urls).unwrap();
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
