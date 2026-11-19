//! The `systemone` format under a round's deadline, against loopback HTTP
//! servers that reply at once, never reply, or stall partway through a body.
#![expect(
    clippy::unwrap_used,
    reason = "the loopback server's helpers fail the test on a broken fixture"
)]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chauffeur_core::Failure;
use chauffeur_judges::{
    AnsweredBy, Evidence, Fallback, Format, KeySource, Model, Probabilities, Question, SystemOne,
    SystemOneEntry,
};

const LIVE_RESPONSE: &str = r#"{"model":"jev-1.13.0","answers":{"q0":{"type":"noul","noul":0.97}},"usage":{"input_tokens":397,"output_tokens":73}}"#;

/// The limit that ends a call, where a test has one end it: the deadline in
/// some tests, the model's timeout in others. The request has to be sent
/// before it runs out, or the server reads none, so this is the stall a
/// loaded runner may put before the send.
const SHORT_MS: u64 = 2_000;
/// The other limit in those tests, five times `SHORT_MS`: a call that lasts
/// this long was not ended by the short one.
const LONG_MS: u64 = 10_000;
/// A bound on a call the short limit ends: 3 s past `SHORT_MS` for a runner
/// late to notice, and half of `LONG_MS`, which a call that ignored the short
/// limit would last.
const SOON: Duration = Duration::from_secs(5);
/// How long a server lives when its test never stops it.
const LIFETIME: Duration = Duration::from_secs(20);
/// How long a server waits on one read or write.
const IO: Duration = Duration::from_secs(5);
/// How long a server sleeps between looks for a connection.
const POLL: Duration = Duration::from_millis(2);

/// What a server does with each request it reads.
#[derive(Clone, Copy)]
enum Reply {
    /// The whole reply, at once: a status and a body.
    Whole(&'static str, &'static str),
    /// Nothing: the connection stays open until the test stops the server.
    Nothing,
    /// The headers and the start of a longer body, then nothing more.
    HalfBody,
    /// The live answer, this long after the request, unless the test has
    /// stopped the server by then.
    Late(Duration),
}

/// A loopback HTTP server on its own thread, which `requests` joins.
struct Server {
    endpoint: String,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<usize>,
}

impl Server {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || serve(&listener, reply, &stopped));
        Self {
            endpoint,
            stop,
            thread,
        }
    }

    /// Stops the server and joins its thread: the number of requests it read.
    fn requests(self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.thread.join().unwrap()
    }
}

/// Answers every connection, keeping each open, until stopped with none
/// waiting or `LIFETIME` has passed. Returns the number of requests read.
fn serve(listener: &TcpListener, reply: Reply, stop: &AtomicBool) -> usize {
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    let mut open = Vec::new();

    loop {
        match listener.accept() {
            Ok((stream, _)) => open.push(answer(stream, reply, stop)),
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::WouldBlock, "accept: {error}");
                if stop.load(Ordering::Acquire) || started.elapsed() >= LIFETIME {
                    break;
                }

                thread::sleep(POLL);
            }
        }
    }

    open.len()
}

/// Reads one request from `stream`, replies as `reply` says, and returns the
/// stream still open.
fn answer(stream: TcpStream, reply: Reply, stop: &AtomicBool) -> TcpStream {
    // An accepted socket can inherit the listener's non-blocking mode.
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(IO)).unwrap();
    stream.set_write_timeout(Some(IO)).unwrap();
    let mut stream = read_request(stream);

    match reply {
        Reply::Whole(status, body) => whole(&mut stream, status, body).unwrap(),
        Reply::Nothing => {}
        Reply::HalfBody => stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"answers\":")
            .unwrap(),
        Reply::Late(delay) => {
            let received = Instant::now();
            while received.elapsed() < delay && !stop.load(Ordering::Acquire) {
                thread::sleep(POLL);
            }

            // A client that has waited this long may leave as the reply is
            // written, which is no fault of the fixture.
            if !stop.load(Ordering::Acquire) {
                let _ = whole(&mut stream, "200 OK", LIVE_RESPONSE);
            }
        }
    }

    stream
}

/// Writes a whole reply: `status` and `body`.
fn whole(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let length = body.len();
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{body}"
    )
}

/// Reads one request from `stream`: JSON that asks a `noul` question.
fn read_request(stream: TcpStream) -> TcpStream {
    let mut reader = BufReader::new(stream);
    let (mut length, mut content_type) = (0, String::new());

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
        if let Some(value) = lower.strip_prefix("content-type:") {
            value.trim().clone_into(&mut content_type);
        }
    }

    let mut request = vec![0; length];
    reader.read_exact(&mut request).unwrap();
    assert_eq!(content_type, "application/json; charset=utf-8");
    let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
    assert_eq!(
        request.pointer("/questions/q0/type"),
        Some(&serde_json::Value::from("noul"))
    );

    reader.into_inner()
}

/// Jev at `server`, with a model timeout of `timeout_ms`.
fn jev(server: &Server, timeout_ms: u64) -> SystemOne {
    let entry = SystemOneEntry {
        endpoint: server.endpoint.clone(),
        model: "jev-latest".into(),
        key: KeySource {
            env: "TYPESAFE_API_KEY".into(),
        },
        timeout_ms: Some(timeout_ms),
    };
    SystemOne::new(&entry, "test-key".into())
}

/// A tier of two Jev models, `first` then `second`.
fn tier(first: SystemOne, second: SystemOne) -> Fallback {
    Fallback::new(vec![
        Model::new("first", Box::new(first)),
        Model::new("second", Box::new(second)),
    ])
}

fn commit_question() -> Question {
    Question::noul(
        "Does this commit message break Conventional Commits?",
        ["commit"],
    )
    .unwrap()
}

fn evidence() -> Evidence {
    Evidence::new([("commit", "fixed stuff")])
}

/// The answer in `LIVE_RESPONSE`.
fn live() -> Probabilities {
    Probabilities::read(commit_question().outcomes(), [("yes", 0.97), ("no", 0.03)]).unwrap()
}

/// A deadline `milliseconds` from now.
fn within(milliseconds: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap()
}

#[test]
fn a_deadline_already_passed_fails_without_a_connection() {
    let server = Server::start(Reply::Whole("200 OK", LIVE_RESPONSE));
    let passed = Some(Instant::now());

    let replies = jev(&server, 5_000).answer_all_until(&[&commit_question()], &evidence(), passed);

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert_eq!(server.requests(), 0);
}

#[test]
fn a_deadline_already_passed_is_a_deadline_whatever_the_models_timeout() {
    // A model with no time of its own times out at once and lets the next
    // model answer. Past the deadline that must not happen: with nothing
    // left, the deadline is the limit, however short the model's timeout.
    let server = Server::start(Reply::Whole("200 OK", LIVE_RESPONSE));
    let (question, evidence) = (commit_question(), evidence());
    let passed = Some(Instant::now());

    let with_time_left =
        jev(&server, 0).answer_all_until(&[&question], &evidence, Some(within(LONG_MS)));
    let with_none_left = jev(&server, 0).answer_all_until(&[&question], &evidence, passed);

    assert_eq!(with_time_left, [Err(Failure::Timeout)]);
    assert_eq!(with_none_left, [Err(Failure::Deadline)]);
    assert_eq!(server.requests(), 0);
}

#[test]
fn the_models_timeout_ends_a_call_with_time_left_as_a_timeout() {
    let server = Server::start(Reply::Nothing);
    let deadline = Some(within(LONG_MS));

    let replies =
        jev(&server, SHORT_MS).answer_all_until(&[&commit_question()], &evidence(), deadline);

    assert_eq!(replies, [Err(Failure::Timeout)]);
    assert_eq!(server.requests(), 1);
}

#[test]
fn the_deadline_ends_a_call_before_the_models_timeout_as_a_deadline() {
    let server = Server::start(Reply::Nothing);
    let started = Instant::now();
    let deadline = Some(within(SHORT_MS));

    let replies =
        jev(&server, LONG_MS).answer_all_until(&[&commit_question()], &evidence(), deadline);
    let took = started.elapsed();

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert!(took < SOON, "{took:?}");
    assert_eq!(server.requests(), 1);
}

#[test]
fn a_body_that_stalls_ends_at_the_deadline_as_a_deadline() {
    let server = Server::start(Reply::HalfBody);
    let started = Instant::now();
    let deadline = Some(within(SHORT_MS));

    let replies =
        jev(&server, LONG_MS).answer_all_until(&[&commit_question()], &evidence(), deadline);
    let took = started.elapsed();

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert!(took < SOON, "{took:?}");
    assert_eq!(server.requests(), 1);
}

#[test]
fn a_body_that_stalls_ends_at_the_models_timeout_as_a_timeout() {
    let (bounded, unbounded) = (
        Server::start(Reply::HalfBody),
        Server::start(Reply::HalfBody),
    );
    let (question, evidence) = (commit_question(), evidence());
    let deadline = Some(within(LONG_MS));

    let with_time_left =
        jev(&bounded, SHORT_MS).answer_all_until(&[&question], &evidence, deadline);
    let with_no_deadline = jev(&unbounded, SHORT_MS).answer_all(&[&question], &evidence);

    assert_eq!(with_time_left, [Err(Failure::Timeout)]);
    assert_eq!(with_no_deadline, [Err(Failure::Timeout)]);
    assert_eq!((bounded.requests(), unbounded.requests()), (1, 1));
}

#[test]
fn a_reply_in_time_reads_the_same_whichever_limit_is_shorter() {
    let cases = [
        ("200 OK", LIVE_RESPONSE, Ok(live())),
        ("408 Request Timeout", "{}", Err(Failure::Timeout)),
        ("429 Too Many Requests", "{}", Err(Failure::RateLimited)),
        ("200 OK", "not json", Err(Failure::Invalid)),
    ];

    for (status, body, expected) in cases {
        // The model's timeout is the shorter limit, then the deadline is.
        for (timeout_ms, deadline_ms) in [(5_000, 60_000), (60_000, 5_000)] {
            let server = Server::start(Reply::Whole(status, body));
            let deadline = Some(within(deadline_ms));

            let replies = jev(&server, timeout_ms).answer_all_until(
                &[&commit_question()],
                &evidence(),
                deadline,
            );

            assert_eq!(
                replies,
                std::slice::from_ref(&expected),
                "{status} {timeout_ms}"
            );
            assert_eq!(server.requests(), 1, "{status} {timeout_ms}");
        }
    }
}

#[test]
fn a_model_that_times_out_with_time_left_falls_back_under_the_same_deadline() {
    let (first, second) = (
        Server::start(Reply::Nothing),
        Server::start(Reply::Whole("200 OK", LIVE_RESPONSE)),
    );
    let fallback = tier(jev(&first, SHORT_MS), jev(&second, 5_000));
    let deadline = Some(within(LONG_MS));

    let replies = fallback.answer_all_until(&[&commit_question()], &evidence(), deadline);

    let by = AnsweredBy {
        model: "second".into(),
        placeholder: false,
    };
    assert_eq!(replies, [Ok((live(), by))]);
    assert_eq!((first.requests(), second.requests()), (1, 1));
}

#[test]
fn a_model_ended_by_the_deadline_is_not_followed_by_the_next() {
    let (first, second) = (
        Server::start(Reply::Nothing),
        Server::start(Reply::Whole("200 OK", LIVE_RESPONSE)),
    );
    let fallback = tier(jev(&first, LONG_MS), jev(&second, 5_000));
    let started = Instant::now();
    let deadline = Some(within(SHORT_MS));

    let replies = fallback.answer_all_until(&[&commit_question()], &evidence(), deadline);
    let took = started.elapsed();

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert!(took < SOON, "{took:?}");
    assert_eq!((first.requests(), second.requests()), (1, 0));
}

#[test]
fn the_next_model_has_only_the_time_the_first_left() {
    // The first model's timeout uses 1000 ms of the 2000. The second replies
    // 1500 ms after it is asked: in time for 2000 ms counted afresh, too late
    // for the 1000 ms that are left. No timer ends early, so that reply is
    // at least 500 ms past the deadline however the runner stalls, and the
    // second model is still asked while the stalls before it total under
    // 1000 ms.
    let (first, second) = (
        Server::start(Reply::Nothing),
        Server::start(Reply::Late(Duration::from_millis(1_500))),
    );
    let fallback = tier(jev(&first, 1_000), jev(&second, LONG_MS));
    let started = Instant::now();
    let deadline = Some(within(2_000));

    let replies = fallback.answer_all_until(&[&commit_question()], &evidence(), deadline);
    let took = started.elapsed();

    assert_eq!(replies, [Err(Failure::Deadline)]);
    assert!(took < SOON, "{took:?}");
    assert_eq!((first.requests(), second.requests()), (1, 1));
}

#[test]
fn the_next_model_answers_late_when_the_first_left_it_the_time() {
    // The same two models with 4000 ms left for the second: its late reply
    // is read, so the test above fails for lack of time and nothing else.
    let (first, second) = (
        Server::start(Reply::Nothing),
        Server::start(Reply::Late(Duration::from_millis(1_500))),
    );
    let fallback = tier(jev(&first, 1_000), jev(&second, LONG_MS));
    let deadline = Some(within(5_000));

    let replies = fallback.answer_all_until(&[&commit_question()], &evidence(), deadline);

    let by = AnsweredBy {
        model: "second".into(),
        placeholder: false,
    };
    assert_eq!(replies, [Ok((live(), by))]);
    assert_eq!((first.requests(), second.requests()), (1, 1));
}
