//! `chauffeur`: runs the daemon, talks to it over its socket, and reads its
//! audit log.

mod audit;
mod navigate;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use chauffeur_daemon::{DaemonError, Paths};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, eyre};
use serde_json::{Value, json};

/// How long a call with no deadline of its own waits for its reply.
const WAIT: Duration = Duration::from_secs(5);
/// How long past its deadline a call waits for the daemon to answer.
const GRACE: Duration = Duration::from_secs(2);
/// The most of a reply line the CLI reads; a longer one is refused.
const MAX_REPLY: usize = 4 * 1024 * 1024;
/// The longest deadline the daemon accepts, the same as
/// `chauffeur_protocol::Limit::Deadline`; the CLI does not depend on the
/// protocol crate, so the bound is repeated here.
const MAX_DEADLINE_MS: u64 = 30_000;

#[derive(Parser)]
#[command(version, about = "A judgment engine for coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon, or ask the running one about itself.
    Daemon {
        #[command(subcommand)]
        command: Daemon,
    },
    /// Print the version.
    Version,
    /// Send one event, read as JSON on stdin, and print what the rules returned.
    Emit {
        /// How long the rules may take, in milliseconds.
        #[arg(long, default_value_t = 3_000)]
        deadline_ms: u64,
    },
    /// Send one navigate request, read as JSON on stdin, and print each
    /// JSON-RPC message that comes back. Exits 0 with an answer or a closed
    /// pipe, 1 when rejected, 2 on daemon or stdout errors.
    Navigate,
    /// Print the most recent runs from the audit log, oldest first.
    Audit {
        /// How many runs.
        #[arg(default_value_t = 20)]
        count: usize,
        /// Keep printing runs as they are written.
        #[arg(long)]
        follow: bool,
        /// One short line per run.
        #[arg(long)]
        brief: bool,
    },
}

#[derive(Subcommand)]
enum Daemon {
    /// Run the daemon in the foreground. Exits at once if one already runs.
    Run {
        /// Shipped models, rules and safety patterns.
        #[arg(long)]
        distribution: Option<PathBuf>,
        /// A models.json used in place of the shipped and your own, such as
        /// placeholders for tests.
        #[arg(long)]
        models: Option<PathBuf>,
    },
    /// Print the running daemon's version, loaded rules and refused rule files.
    Status,
}

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    let name = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "chauffeur".to_owned());
    match Cli::parse().command {
        Command::Daemon {
            command:
                Daemon::Run {
                    distribution,
                    models,
                },
        } => match chauffeur_daemon::run(&daemon_paths(&name, distribution, models)) {
            Err(DaemonError::Running(socket)) => {
                eprintln!(
                    "{name}: a daemon is already running on {}",
                    socket.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => Err(error.into()),
        },
        Command::Daemon {
            command: Daemon::Status,
        } => print(&call(&name, "chauffeur.status", &Value::Null, WAIT)?),
        Command::Version => print_line(
            &mut io::stdout().lock(),
            &format!("{name} {}", env!("CARGO_PKG_VERSION")),
        ),
        Command::Emit { deadline_ms } => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            let event: Value =
                serde_json::from_str(&text).wrap_err("the event on stdin is not JSON")?;
            let deadline = Duration::from_millis(deadline_ms.min(MAX_DEADLINE_MS));
            let wait = deadline.saturating_add(GRACE);

            print(&call(
                &name,
                "chauffeur.emit",
                &json!({ "event": event, "deadline_ms": deadline_ms }),
                wait,
            )?)
        }
        Command::Navigate => {
            let socket = Paths::from_env(&name, None).socket;
            Ok(navigate::navigate(&socket, &mut std::io::stdin(), &mut std::io::stdout()).into())
        }
        Command::Audit {
            count,
            follow,
            brief,
        } => {
            audit::print(&Paths::from_env(&name, None).state, count, follow, brief)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The daemon's paths for `name`, with `models` used in place of the
/// shipped and the user's models files.
fn daemon_paths(name: &str, distribution: Option<PathBuf>, models: Option<PathBuf>) -> Paths {
    Paths {
        models,
        ..Paths::from_env(name, distribution)
    }
}

fn print(result: &Value) -> Result<ExitCode> {
    print_line(
        &mut io::stdout().lock(),
        &serde_json::to_string_pretty(result)?,
    )
}

/// Prints `text` and a newline to `out`. A reader that stops early, as
/// `head` does, closes the pipe; printing then stops, and succeeds. Any
/// other failed write is an error.
fn print_line(out: &mut impl Write, text: &str) -> Result<ExitCode> {
    let printed = writeln!(out, "{text}").and_then(|()| out.flush());

    match printed {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
            Err(error).wrap_err("cannot print the output")
        }
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// One JSON-RPC call to the daemon, returning its result. `wait` bounds the
/// answer, so no call waits on a daemon that has stopped answering.
fn call(name: &str, method: &str, params: &Value, wait: Duration) -> Result<Value> {
    let socket = Paths::from_env(name, None).socket;
    let mut stream = UnixStream::connect(&socket)
        .wrap_err_with(|| format!("no daemon answers on {}", socket.display()))?;
    writeln!(
        stream,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    )?;
    answer(&stream, wait)
}

/// The daemon's one-line answer to `stream`, read within `wait` end to end:
/// a daemon that trickles bytes cannot extend the wait.
fn answer(stream: &UnixStream, wait: Duration) -> Result<Value> {
    let (sender, receiver) = mpsc::channel();
    let line = thread::scope(|scope| {
        let _reader = scope.spawn(move || {
            let result = stream.try_clone().and_then(|stream| {
                let mut line = Vec::new();
                let limit = u64::try_from(MAX_REPLY)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);

                BufReader::new(stream)
                    .take(limit)
                    .read_until(b'\n', &mut line)?;

                Ok(line)
            });
            let _ = sender.send(result);
        });

        match receiver.recv_timeout(wait) {
            Ok(Ok(line)) => Ok(line),
            Ok(Err(error)) => Err(error).wrap_err("cannot read the daemon's answer"),
            Err(RecvTimeoutError::Timeout) => {
                let _ = stream.shutdown(Shutdown::Both);
                Err(eyre!("the daemon did not answer in time"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(eyre!("the reader stopped before it sent the answer"))
            }
        }
    })?;

    if line.len() > MAX_REPLY {
        return Err(eyre!("the daemon's answer is over 4 MiB"));
    }

    let reply: Value = serde_json::from_slice(&line).wrap_err("the daemon's reply is not JSON")?;
    if let Some(error) = reply.get("error") {
        let message = error.get("message").and_then(Value::as_str);
        return Err(eyre!(
            "{}",
            message.unwrap_or("the daemon refused the call")
        ));
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::thread;
    use std::time::{Duration, Instant};

    use color_eyre::eyre::Result;
    use serde_json::Value;

    use super::{answer, daemon_paths, print_line};

    /// A writer whose every write fails with `kind`.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_line_is_printed_with_its_newline() {
        let mut out = Vec::new();

        let printed = print_line(&mut out, "chauffeur 1.0.0");

        assert_eq!(printed.ok(), Some(ExitCode::SUCCESS));
        assert_eq!(out, b"chauffeur 1.0.0\n");
    }

    #[test]
    fn a_closed_pipe_stops_printing_with_success() {
        let printed = print_line(&mut Failing(io::ErrorKind::BrokenPipe), "text");

        assert_eq!(printed.ok(), Some(ExitCode::SUCCESS));
    }

    #[test]
    fn any_other_failed_write_is_an_error() {
        let printed = print_line(&mut Failing(io::ErrorKind::Other), "text");

        let error = printed.err().map(|error| error.to_string());
        assert_eq!(error.as_deref(), Some("cannot print the output"));
    }

    #[test]
    fn a_models_file_is_used_in_place_of_the_shipped_and_the_users() {
        let models = PathBuf::from("/tmp/chauffeur-models.json");

        let paths = daemon_paths("chauffeur", None, Some(models.clone()));

        assert_eq!(paths.models, Some(models));
    }

    /// A daemon that never answers must not hold the command: the wait ends
    /// it, with the same failure a refused call gets. The peer keeps its end
    /// open for a few seconds and then drops it, so a wait that never returns
    /// fails the test on the message instead of hanging it.
    #[test]
    fn an_answer_that_never_comes_is_refused_at_the_wait() {
        let (ours, peer) = UnixStream::pair().unwrap();
        let _silent = thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            drop(peer);
        });
        let started = Instant::now();

        let error = answer(&ours, Duration::from_millis(100)).unwrap_err();

        assert_eq!(error.to_string(), "the daemon did not answer in time");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?} waiting",
            started.elapsed()
        );
    }

    /// A reply line of `size` bytes, its newline included, that parses as a
    /// JSON-RPC result with a `pad` of the padding, which it returns too.
    fn reply_of(size: usize) -> (Vec<u8>, usize) {
        const HEAD: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"pad":""#;
        const TAIL: &str = r#""}}"#;
        let padding = size
            .saturating_sub(HEAD.len())
            .saturating_sub(TAIL.len())
            .saturating_sub(1);
        let mut line = Vec::with_capacity(size);
        line.extend_from_slice(HEAD.as_bytes());
        line.extend(std::iter::repeat_n(b'a', padding));
        line.extend_from_slice(TAIL.as_bytes());
        line.push(b'\n');
        assert_eq!(line.len(), size, "the reply line is the size asked for");

        (line, padding)
    }

    /// `answer` over a pair whose peer writes `line` and closes. The pair's
    /// buffer is small, so the peer needs its own thread to write a long one.
    /// The peer's write is bounded and its failure ignored: when the read
    /// stops early, the peer cannot finish writing, so the read end is shut
    /// down and closed before the join, and the test fails on its assertion
    /// instead of on a join that never returns.
    fn answered(line: Vec<u8>, wait: Duration) -> Result<Value> {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let writer = thread::spawn(move || {
            let _ = peer.write_all(&line);
        });
        let answer = answer(&ours, wait);
        let _ = ours.shutdown(Shutdown::Both);
        drop(ours);
        writer.join().unwrap();

        answer
    }

    /// A daemon that writes its reply and closes at once is still read: the
    /// peer is gone before `answer` starts.
    #[test]
    fn a_reply_from_a_peer_that_already_closed_is_returned() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let (line, padding) = reply_of(64);
        peer.write_all(&line).unwrap();
        drop(peer);

        let reply = answer(&ours, Duration::from_secs(5)).unwrap();

        assert_eq!(reply["pad"].as_str().map(str::len), Some(padding));
    }

    /// A daemon that trickles bytes without ever finishing its line cannot
    /// extend the wait: the whole reply is bounded, not each read.
    #[test]
    fn a_trickled_answer_ends_at_the_wait() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..100 {
                if peer.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();

        let error = answer(&ours, Duration::from_millis(100)).unwrap_err();
        let waited = started.elapsed();
        let _ = ours.shutdown(Shutdown::Both);
        drop(ours);
        writer.join().unwrap();

        assert_eq!(error.to_string(), "the daemon did not answer in time");
        assert!(waited < Duration::from_secs(1), "{waited:?} waiting");
    }

    /// The last byte the bound admits is accepted, not refused: a line of
    /// exactly 4 MiB, its newline included, is read whole and parsed.
    #[test]
    fn a_reply_line_of_exactly_four_mebibytes_is_accepted() {
        // The CLI's own 4 MiB, written out rather than read from `MAX_REPLY`:
        // the test must hold the boundary still while a mutant moves the
        // constant under it.
        let size = 4 * 1024 * 1024;
        let (line, padding) = reply_of(size);

        let reply = answered(line, Duration::from_secs(5)).unwrap();

        assert_eq!(reply["pad"].as_str().map(str::len), Some(padding));
    }

    /// One byte more is refused, however valid the JSON behind it.
    #[test]
    fn a_reply_line_one_byte_over_four_mebibytes_is_refused() {
        // As above: the test owns its 4 MiB, not `MAX_REPLY`.
        let size = 4 * 1024 * 1024 + 1;
        let (line, _) = reply_of(size);

        let error = answered(line, Duration::from_secs(5)).unwrap_err();

        assert_eq!(error.to_string(), "the daemon's answer is over 4 MiB");
    }
}
