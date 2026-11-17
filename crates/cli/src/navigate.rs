//! `chauffeur navigate`: one request on stdin, the JSON-RPC messages that
//! come back on stdout, one per line, and an exit status a script can test.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
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
fn relay(
    stream: &UnixStream,
    mut reader: impl BufRead,
    until: Option<Instant>,
    output: &mut impl Write,
) -> Ended {
    let mut line = String::new();
    loop {
        let left = until.map(|until| until.saturating_duration_since(Instant::now()));
        // A zero timeout is refused, which is how a passed deadline shows.
        if stream.set_read_timeout(left).is_err() {
            return complain("the daemon did not answer in time", Ended::Unavailable);
        }
        line.clear();
        match (&mut reader)
            .take(MAX_REPLY.saturating_add(1))
            .read_line(&mut line)
        {
            Ok(0) => break,
            Ok(_) if line.len() > usize::try_from(MAX_REPLY).unwrap_or(usize::MAX) => {
                return complain("the daemon's reply exceeds 4 MiB", Ended::Unavailable);
            }
            Ok(_) => {}
            Err(error) => return complain(&read_failure(&error), Ended::Unavailable),
        }
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
        let (stream, _peer) = UnixStream::pair().unwrap();
        for (kind, expected) in [
            (io::ErrorKind::BrokenPipe, Ended::Answered),
            (io::ErrorKind::PermissionDenied, Ended::Unavailable),
        ] {
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
