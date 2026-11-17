//! `chauffeur navigate`: one request on stdin, the JSON-RPC messages that
//! come back on stdout, one per line, and an exit status a script can test.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How much longer than the request's deadline the daemon is waited for.
const MARGIN: Duration = Duration::from_secs(2);

/// The deadline the daemon gives a request that names none.
const DEFAULT_DEADLINE_MS: u64 = 3_000;
const MAX_DEADLINE_MS: u64 = 30_000;
const MAX_REPLY: u64 = 4 * 1024 * 1024;

/// The ID of the one request sent.
const ID: u64 = 1;

/// How a navigation ended, as the command's exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Intents or a verdict came back: 0.
    Answered,
    /// The daemon rejected the request: 1.
    Rejected,
    /// No daemon answered, or it speaks another protocol: 2.
    Unavailable,
}

impl From<Ended> for ExitCode {
    fn from(ended: Ended) -> Self {
        match ended {
            Ended::Answered => Self::SUCCESS,
            Ended::Rejected => Self::from(1),
            Ended::Unavailable => Self::from(2),
        }
    }
}

/// Sends the request read from `input` to the daemon on `socket` and
/// writes each message it sends back to `output`, until the reply.
pub fn navigate(socket: &Path, input: &mut impl Read, output: &mut impl Write) -> Ended {
    let mut text = Vec::new();
    let limit = u64::try_from(chauffeur_daemon::MAX_LINE).unwrap_or(u64::MAX);
    let read = input.take(limit.saturating_add(1)).read_to_end(&mut text);
    if text.len() > chauffeur_daemon::MAX_LINE {
        return complain("request over 1 MiB", Ended::Rejected);
    }
    let params: Value = match read.map(|_| serde_json::from_slice(&text)) {
        Ok(Ok(params)) => params,
        Ok(Err(error)) => {
            return complain(
                &format!("the request on stdin is not JSON: {error}"),
                Ended::Rejected,
            );
        }
        Err(error) => return complain(&format!("cannot read stdin: {error}"), Ended::Rejected),
    };
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return complain(
            &format!("no daemon answers on {}", socket.display()),
            Ended::Unavailable,
        );
    };
    let deadline = params
        .get("deadline_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEADLINE_MS)
        .min(MAX_DEADLINE_MS);
    let wait = Duration::from_millis(deadline).saturating_add(MARGIN);
    let until = Instant::now().checked_add(wait);
    let request =
        json!({ "jsonrpc": "2.0", "id": ID, "method": "chauffeur.navigate", "params": params });
    if writeln!(stream, "{request}").is_err() {
        return complain("the daemon closed the connection", Ended::Unavailable);
    }
    let Ok(reader) = stream.try_clone() else {
        return complain("cannot read from the daemon", Ended::Unavailable);
    };
    relay(&stream, BufReader::new(reader), until, output)
}

/// Copies each message to `output` until the reply to the request, or
/// `until`, however many events arrive before it.
///
/// A scoped reader hands over each line as it comes, so a reply from a peer
/// that closes at once is read without relying on a socket timeout, which
/// fails once the peer is gone. One deadline bounds the whole reply, and
/// shutting the stream down ends a read the relay no longer waits for.
fn relay(
    stream: &UnixStream,
    reader: impl BufRead + Send,
    until: Option<Instant>,
    output: &mut impl Write,
) -> Ended {
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(1);
        let _reader = scope.spawn(move || read_lines(reader, &sender));
        let ended = forward(&receiver, until, output);
        // The reader may be in a read the relay no longer waits for.
        let _ = stream.shutdown(Shutdown::Both);

        ended
    })
}

/// One thing the reader sent: the daemon's next line, or why no more come.
enum Incoming {
    /// A line, without its newline.
    Line(String),
    /// A line over `MAX_REPLY`, which is never printed.
    OverLimit,
    /// The read itself failed.
    Failed(io::Error),
}

/// Reads the daemon's lines, each at most `MAX_REPLY + 1` bytes, sending
/// each to `sender` and returning at the end of the input, an over-limit
/// line, a read error, or a relay that stopped listening.
fn read_lines(mut reader: impl BufRead, sender: &SyncSender<Incoming>) {
    let limit = MAX_REPLY.saturating_add(1);
    let over = usize::try_from(MAX_REPLY).unwrap_or(usize::MAX);
    let mut line = String::new();
    loop {
        line.clear();
        match (&mut reader).take(limit).read_line(&mut line) {
            Ok(0) => return,
            Ok(_) if line.len() > over => {
                let _ = sender.send(Incoming::OverLimit);
                return;
            }
            Ok(_) => {
                if sender
                    .send(Incoming::Line(std::mem::take(&mut line)))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Incoming::Failed(error));
                return;
            }
        }
    }
}

/// Writes each line the reader sends until the reply to the request, the
/// deadline, or the end of the daemon's output.
fn forward(
    receiver: &Receiver<Incoming>,
    until: Option<Instant>,
    output: &mut impl Write,
) -> Ended {
    loop {
        let left = until.map(|until| until.saturating_duration_since(Instant::now()));
        let arrived = left.map_or_else(
            || receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
            |left| receiver.recv_timeout(left),
        );
        let line = match arrived {
            Ok(Incoming::Line(line)) => line,
            Ok(Incoming::OverLimit) => {
                return complain("the daemon's reply exceeds 4 MiB", Ended::Unavailable);
            }
            Ok(Incoming::Failed(error)) => {
                return complain(&read_failure(&error), Ended::Unavailable);
            }
            Err(RecvTimeoutError::Timeout) => {
                return complain("the daemon did not answer in time", Ended::Unavailable);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let text = line.trim_end();
        // A reader that went away, such as `head -1`, has what it wanted.
        if let Err(error) = writeln!(output, "{text}") {
            return if error.kind() == io::ErrorKind::BrokenPipe {
                Ended::Answered
            } else {
                complain(
                    &format!("cannot write to stdout: {error}"),
                    Ended::Unavailable,
                )
            };
        }
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if message.get("id") != Some(&json!(ID)) {
            continue;
        }
        let reason = message
            .pointer("/error/data/reason")
            .and_then(Value::as_str);
        return match (message.get("result"), reason) {
            (Some(_), _) => Ended::Answered,
            (None, Some("unsupported_protocol")) => Ended::Unavailable,
            (None, _) => Ended::Rejected,
        };
    }
    complain(
        "the daemon closed the connection without answering",
        Ended::Unavailable,
    )
}

/// Why reading from the daemon failed: a missed deadline, or anything else.
fn read_failure(error: &io::Error) -> String {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        "the daemon did not answer in time".to_owned()
    } else {
        format!("cannot read from the daemon: {error}")
    }
}

fn complain(message: &str, ended: Ended) -> Ended {
    let _ = writeln!(io::stderr().lock(), "chauffeur navigate: {message}");
    ended
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailedWriter(io::ErrorKind);

    impl Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn relay_distinguishes_closed_pipe_from_other_write_errors() {
        for (kind, expected) in [
            (io::ErrorKind::BrokenPipe, Ended::Answered),
            (io::ErrorKind::PermissionDenied, Ended::Unavailable),
        ] {
            let (stream, _peer) = UnixStream::pair().unwrap();
            let reader = BufReader::new(&b"{}\n"[..]);
            let until = Instant::now().checked_add(Duration::from_secs(1));
            assert_eq!(
                relay(&stream, reader, until, &mut FailedWriter(kind)),
                expected
            );
        }
    }

    #[test]
    fn request_over_one_mib_is_rejected_before_connecting() {
        let bytes = vec![b' '; chauffeur_daemon::MAX_LINE.saturating_add(1)];
        let mut input = bytes.as_slice();
        assert_eq!(
            navigate(Path::new("/missing"), &mut input, &mut Vec::new()),
            Ended::Rejected
        );
    }

    /// The reply limit as written, a literal so that editing `MAX_REPLY`
    /// fails these tests instead of moving their target with it.
    const REPLY_LIMIT: usize = 4 * 1024 * 1024;

    /// A reply to the request, padded with spaces inside its empty result so
    /// that, with its newline, it is `size` bytes.
    fn padded_reply(size: usize) -> String {
        let opening = r#"{"jsonrpc":"2.0","id":1,"result":{"#;
        let closing = "}}\n";
        let padding = size
            .saturating_sub(opening.len())
            .saturating_sub(closing.len());
        format!("{opening}{}{closing}", " ".repeat(padding))
    }

    #[test]
    fn a_reply_exactly_at_the_limit_is_answered() {
        let line = padded_reply(REPLY_LIMIT);
        assert_eq!(line.len(), REPLY_LIMIT);
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut output = Vec::new();
        assert_eq!(
            relay(&stream, BufReader::new(line.as_bytes()), None, &mut output),
            Ended::Answered
        );
        assert_eq!(output.as_slice(), line.as_bytes());
    }

    #[test]
    fn a_reply_one_byte_over_the_limit_is_refused_and_never_printed() {
        let size = REPLY_LIMIT.saturating_add(1);
        let line = padded_reply(size);
        assert_eq!(line.len(), size);
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut output = Vec::new();
        assert_eq!(
            relay(&stream, BufReader::new(line.as_bytes()), None, &mut output),
            Ended::Unavailable
        );
        assert_eq!(output, Vec::<u8>::new());
    }

    /// `relay` over one end of a socket pair, read through a clone as
    /// `navigate` reads the daemon's lines.
    fn relayed(ours: &UnixStream, until: Option<Instant>, output: &mut impl Write) -> Ended {
        relay(
            ours,
            BufReader::new(ours.try_clone().unwrap()),
            until,
            output,
        )
    }

    /// A daemon past its connection limit writes its rejection and closes at
    /// once: the line is still read, and its reason is the exit status.
    #[test]
    fn a_rejection_from_a_peer_that_already_closed_is_relayed() {
        let rejection = concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"#,
            r#""message":"the daemon is busy","data":{"reason":"busy"}}}"#
        );
        let (ours, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(format!("{rejection}\n").as_bytes()).unwrap();
        drop(peer);
        let mut output = Vec::new();

        assert_eq!(relayed(&ours, None, &mut output), Ended::Rejected);

        assert_eq!(output.as_slice(), format!("{rejection}\n").as_bytes());
    }

    /// A daemon that never answers must not hold the command: the deadline
    /// ends it. The peer keeps its end open past the assertion and then
    /// drops it, so a wait that never returns fails on the message instead
    /// of hanging the test.
    #[test]
    fn a_silent_peer_ends_at_the_deadline() {
        let (ours, peer) = UnixStream::pair().unwrap();
        let _silent = thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            drop(peer);
        });
        let until = Instant::now().checked_add(Duration::from_millis(100));
        let mut output = Vec::new();
        let started = Instant::now();

        assert_eq!(relayed(&ours, until, &mut output), Ended::Unavailable);

        let waited = started.elapsed();
        assert!(waited < Duration::from_secs(1), "{waited:?} waiting");
        assert_eq!(output, Vec::<u8>::new());
    }

    /// A daemon that trickles bytes without ever finishing its line cannot
    /// extend the deadline: the whole reply is bounded, not each read.
    #[test]
    fn a_trickled_reply_ends_at_the_deadline() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..100 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let until = Instant::now().checked_add(Duration::from_millis(100));
        let mut output = Vec::new();
        let started = Instant::now();

        let ended = relayed(&ours, until, &mut output);
        let waited = started.elapsed();
        drop(ours);
        writer.join().unwrap();

        assert_eq!(ended, Ended::Unavailable);
        assert!(waited < Duration::from_secs(1), "{waited:?} waiting");
        assert_eq!(output, Vec::<u8>::new());
    }

    #[test]
    fn a_read_failure_tells_a_missed_deadline_from_a_broken_read() {
        for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
            assert_eq!(
                read_failure(&io::Error::from(kind)),
                "the daemon did not answer in time"
            );
        }
        let broken = io::Error::from(io::ErrorKind::ConnectionReset);
        assert_eq!(
            read_failure(&broken),
            format!("cannot read from the daemon: {broken}")
        );
    }

    /// `{}` followed by spaces, so it is `size` bytes and still JSON.
    fn padded_request(size: usize) -> Vec<u8> {
        let mut bytes = b"{}".to_vec();
        bytes.resize(size, b' ');
        bytes
    }

    #[test]
    fn a_request_exactly_at_the_limit_reaches_the_socket() {
        let bytes = padded_request(chauffeur_daemon::MAX_LINE);
        let mut input = bytes.as_slice();
        assert_eq!(
            navigate(
                Path::new("/nonexistent/chauffeur.sock"),
                &mut input,
                &mut Vec::new()
            ),
            Ended::Unavailable
        );
    }

    #[test]
    fn a_request_one_byte_over_the_limit_is_rejected() {
        let bytes = padded_request(chauffeur_daemon::MAX_LINE.saturating_add(1));
        let mut input = bytes.as_slice();
        assert_eq!(
            navigate(
                Path::new("/nonexistent/chauffeur.sock"),
                &mut input,
                &mut Vec::new()
            ),
            Ended::Rejected
        );
    }
}
