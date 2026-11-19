//! A loopback HTTP server that serves one request, for the tests of the
//! `systemone` format.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chauffeur_core::Failure;
use chauffeur_judges::Probabilities;

/// How long a server waits for its request, and on each read of it. A test
/// whose request never comes fails when it joins the server, and cannot hang.
const WAIT: Duration = Duration::from_secs(5);
/// How long a server sleeps between looks for a connection.
const POLL: Duration = Duration::from_millis(2);

/// What the server received: the path, the authorization header and the body.
pub struct Received {
    pub path: String,
    pub authorization: String,
    pub body: serde_json::Value,
}

/// The one connection `listener` is sent, or a panic once `WAIT` has passed
/// with none.
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // An accepted socket can inherit the listener's non-blocking mode.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(WAIT)).unwrap();
                return stream;
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::WouldBlock, "accept: {error}");
                assert!(started.elapsed() < WAIT, "no request within {WAIT:?}");
                thread::sleep(POLL);
            }
        }
    }
}

/// Serves one request with `status` and `body`, after `delay`.
pub fn serve(
    status: &'static str,
    body: &'static str,
    delay: Duration,
) -> (String, JoinHandle<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut reader = BufReader::new(accept(&listener));
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let (mut authorization, mut length) = (String::new(), 0);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            if lower.starts_with("authorization:") {
                line.trim().clone_into(&mut authorization);
            }
        }
        let mut request = vec![0; length];
        reader.read_exact(&mut request).unwrap();
        thread::sleep(delay);
        let _ = write!(
            reader.get_mut(),
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        Received {
            path: request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned(),
            authorization,
            body: serde_json::from_slice(&request).unwrap(),
        }
    });
    (endpoint, server)
}

/// Each reply's probabilities in thousandths, in its question's outcome
/// order, or its failure.
pub fn thousandths(replies: Vec<Result<Probabilities, Failure>>) -> Vec<Result<Vec<u16>, Failure>> {
    replies
        .into_iter()
        .map(|reply| reply.map(|p| p.as_slice().iter().map(|p| p.get()).collect()))
        .collect()
}
