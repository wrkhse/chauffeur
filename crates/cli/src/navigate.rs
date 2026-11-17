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
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return complain("the daemon did not answer in time", Ended::Unavailable);
            }
            Err(error) => {
                return complain(
                    &format!("cannot read from the daemon: {error}"),
                    Ended::Unavailable,
                );
            }
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
}
