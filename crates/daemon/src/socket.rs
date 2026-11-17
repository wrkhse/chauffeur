use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::DropGuard;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_protocol::Rejection;
use chauffeur_runtime::{Cancel, Permit, Source};
use serde_json::{Value, json};

use crate::DaemonError;
use crate::paths::private_folder;
use crate::route::Router;
use crate::rpc;

/// The longest request line accepted.
pub const MAX_LINE: usize = 1 << 20;
/// The most connections served at once.
pub const MAX_CONNECTIONS: usize = 64;
/// The most calls open on one connection.
pub const MAX_CALLS: usize = 16;
/// One client leaves half the integration share to other connections.
pub const MAX_NAVIGATIONS: usize = 4;
const BUSY_WRITE: Duration = Duration::from_millis(100);
const WRITE: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Binds the socket in its private folder, refusing a live daemon.
///
/// # Errors
///
/// Running, a folder that is not private, or a socket that cannot bind.
pub fn bind(socket: &Path) -> Result<UnixListener, DaemonError> {
    private_folder(socket)?;

    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            return Err(DaemonError::Running(socket.to_owned()));
        }

        std::fs::remove_file(socket).map_err(|source| DaemonError::Socket {
            path: socket.to_owned(),
            source,
        })?;
    }

    UnixListener::bind(socket).map_err(|source| DaemonError::Socket {
        path: socket.to_owned(),
        source,
    })
}

/// Serves bounded connections; failed accepts retry after a short backoff.
pub fn serve(listener: &UnixListener, router: &Arc<Router>) -> ! {
    let open = Arc::new(AtomicUsize::new(0));

    loop {
        let stream = accept_next(
            || listener.accept().map(|(stream, _)| stream),
            |error| eprintln!("chauffeur: cannot accept a connection: {error}; retrying"),
        );
        let taken = open.fetch_add(1, Ordering::SeqCst);
        let slot = DropGuard::new(Arc::clone(&open), |open| {
            open.fetch_sub(1, Ordering::SeqCst);
        });

        if taken >= MAX_CONNECTIONS {
            drop(slot);
            busy(stream);
            continue;
        }

        let router = Arc::clone(router);
        let _ = thread::Builder::new()
            .name("connection".into())
            .spawn(move || {
                connection(&router, stream);
                drop(slot);
            });
    }
}

/// Logs the first failed accept in a row and retries with backoff.
fn accept_next<S>(mut accept: impl FnMut() -> io::Result<S>, mut log: impl FnMut(&io::Error)) -> S {
    let mut failed = false;

    loop {
        match accept() {
            Ok(stream) => return stream,
            Err(error) if !failed => {
                log(&error);
                failed = true;
            }
            Err(_) => {}
        }

        thread::sleep(ACCEPT_RETRY);
    }
}

fn busy(mut stream: UnixStream) {
    let reply = busy_reply(
        &Value::Null,
        &format!("the daemon is busy: it serves {MAX_CONNECTIONS} connections already"),
    );

    if stream.set_write_timeout(Some(BUSY_WRITE)).is_ok() {
        let _ = writeln!(stream, "{reply}");
    }
}

/// An error reply to `id` saying the daemon is busy, with `message` in place
/// of the one it carries.
fn busy_reply(id: &Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": busy_error(message) })
}

/// Busy's error, with `message` in place of the one it carries.
fn busy_error(message: &str) -> Value {
    let mut error = Rejection::Busy.to_error();
    if let Some(fields) = error.as_object_mut() {
        fields.insert("message".into(), json!(message));
    }
    error
}

/// Reads and admits on this thread; each admitted call gets a fallible
/// scoped thread. A notification runs nothing, with no work and no reply,
/// except `$/cancelRequest`, which this thread handles.
fn connection(router: &Router, stream: UnixStream) {
    serve_connection(router, stream, IDLE);
}

/// Serves one connection until its peer closes, its socket fails, or it is
/// idle for `idle` with no call open.
///
/// Every read waits at most `idle`, whether or not a call is open. A
/// timeout is a decision point: with no call open the connection is idle and
/// ends, and with a call open the read simply starts again, so a client that
/// sent a call and went silent keeps its thread only while that call runs. A
/// line read so far is kept across a timeout.
fn serve_connection(router: &Router, stream: UnixStream, idle: Duration) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let wire = Wire {
        writer: Mutex::new(writer),
        dead: AtomicBool::new(false),
    };
    let calls = Calls::default();

    thread::scope(|scope| {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        // A blocking read would hold this thread until the peer hung up, so
        // the read stays bounded for the connection's whole life.
        let _ = reader.get_ref().set_read_timeout(Some(idle));

        loop {
            match next(&mut reader, &mut line, &wire) {
                Next::Request(request) => {
                    if request.method == rpc::CANCEL {
                        calls.cancel(request.params.get("id"));
                        continue;
                    }

                    // A notification runs nothing.
                    let Some(id) = request.id.clone() else {
                        continue;
                    };

                    let navigate = request.method == "chauffeur.navigate";
                    let prepared = calls
                        .room(&id, navigate)
                        .and_then(|()| rpc::prepare(router, request));
                    let call = match prepared {
                        Ok(call) => call,
                        Err(refused) => {
                            let _ = wire.send(&refused);
                            continue;
                        }
                    };
                    let slot = calls.open(id.clone(), navigate);
                    let (calls, wire) = (&calls, &wire);
                    let started =
                        thread::Builder::new()
                            .name("call".into())
                            .spawn_scoped(scope, move || {
                                finish_call(router, calls, wire, slot, call);
                            });

                    if let Err(error) = started {
                        // A spawn that fails runs nothing: dropping the
                        // closure drops the call's slot with it.
                        let _ = wire.send(&busy_reply(
                            &id,
                            &format!("the daemon is busy: cannot start a call thread: {error}"),
                        ));
                    }
                }
                Next::Idle if calls.is_empty() => break,
                Next::Idle => {}
                Next::Oversized => {
                    oversized(&mut reader, &line, &wire);
                    break;
                }
                Next::Failed(error) => {
                    eprintln!("chauffeur: a connection's read failed: {error}");
                    break;
                }
                Next::Ended => break,
            }
        }

        calls.cancel_all();
    });
}

fn finish_call(router: &Router, calls: &Calls, wire: &Wire, slot: Slot<'_>, call: rpc::Call) {
    let notify = |message: Value| {
        if wire.send(&message).is_err() {
            calls.cancel_all();
        }
    };

    answer_and_reply(calls, wire, slot, |cancel| {
        call.answer(router, cancel, &notify)
    });
}

/// Answers the call holding `slot` and sends its reply. A call that panics
/// is answered as cancelled when its cancel was set, and as an internal
/// error when it was not, in place of its own reply, so its caller hears
/// back and the connection keeps working; the slot frees its place as it
/// drops.
fn answer_and_reply(
    calls: &Calls,
    wire: &Wire,
    slot: Slot<'_>,
    answer: impl FnOnce(&Cancel) -> (Option<Value>, Option<Permit>),
) {
    let answered = panic::catch_unwind(AssertUnwindSafe(|| answer(slot.cancel())));
    let (reply, permit) = answered.unwrap_or_else(|_| {
        // This line carries no payload; the default panic hook has already
        // printed it to stderr, so the payload is whatever the hook wrote.
        let cancelled = slot.cancel().is_cancelled();
        let outcome = if cancelled {
            "cancelled"
        } else {
            "an internal error"
        };
        eprintln!(
            "chauffeur: a call panicked: answered as {outcome}; \
             the payload was printed by the panic hook"
        );
        let reply = if cancelled {
            json!({
                "jsonrpc": "2.0",
                "id": slot.id(),
                "error": Rejection::Cancelled.to_error(),
            })
        } else {
            rpc::error(slot.id(), -32_603, "internal error")
        };
        (Some(reply), None)
    });
    // Late workers retain their copies; settled calls release before reply.
    drop(permit);
    // The place is given back before the reply, so its ID may be reused by
    // the next line.
    drop(slot);

    if let Some(reply) = reply
        && wire.send(&reply).is_err()
    {
        calls.cancel_all();
    }
}

/// What one read found.
enum Next {
    /// A request line, read whole.
    Request(rpc::Request),
    /// The idle timeout passed with no complete line; what arrived is kept
    /// in the line buffer for the next read.
    Idle,
    /// A line was over the limit; the caller answers it and ends the
    /// connection.
    Oversized,
    /// The connection ended: its peer closed.
    Ended,
    /// A read failed; the connection ends, and the caller logs `error`.
    Failed(io::Error),
}

/// What one read returned.
enum ReadOutcome {
    /// The read returned, with the byte count it read; zero means the peer
    /// closed.
    Line(usize),
    /// The read timeout passed before a complete line arrived.
    TimedOut,
    /// The read failed; the connection ends.
    Failed(io::Error),
}

/// Sorts what one read returned: bytes, the read timeout, or a failure.
fn classify(result: io::Result<usize>) -> ReadOutcome {
    match result {
        Ok(read) => ReadOutcome::Line(read),
        Err(error) if timed_out(&error) => ReadOutcome::TimedOut,
        Err(error) => ReadOutcome::Failed(error),
    }
}

/// Reads one request line, answering malformed lines itself. An idle
/// timeout comes back apart from an end or a failed read, so the caller can
/// decide whether the connection has gone idle; a line over the limit comes
/// back as [`Next::Oversized`] for the caller to answer.
///
/// The line buffer is cleared only once a request is read: a partial line
/// read before a timeout stays there, so a slow line is not lost.
fn next(reader: &mut impl BufRead, line: &mut Vec<u8>, wire: &Wire) -> Next {
    loop {
        let left = MAX_LINE.saturating_add(1).saturating_sub(line.len());
        let limit = u64::try_from(left.max(1)).unwrap_or(u64::MAX);

        match classify(reader.by_ref().take(limit).read_until(b'\n', line)) {
            ReadOutcome::TimedOut => return Next::Idle,
            ReadOutcome::Line(0) => return Next::Ended,
            ReadOutcome::Failed(error) => return Next::Failed(error),
            ReadOutcome::Line(_) => {}
        }

        if line.len() > MAX_LINE {
            return Next::Oversized;
        }

        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim();
        let parsed = if trimmed.is_empty() {
            None
        } else {
            Some(rpc::parse(trimmed))
        };
        line.clear();

        // A malformed line is answered and the next one read; a write that
        // fails ends the connection.
        match parsed {
            Some(Ok(request)) => return Next::Request(request),
            Some(Err(reply)) if wire.send(&reply).is_err() => return Next::Ended,
            None | Some(Err(_)) => {}
        }
    }
}

/// Whether a read error is the socket's read timeout rather than a failure.
fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Answers a line over the limit, reading through its newline so a writer is
/// not left blocked on a daemon that stopped reading; the caller ends the
/// connection.
fn oversized(reader: &mut BufReader<UnixStream>, line: &[u8], wire: &Wire) {
    let reply = rpc::error(&Value::Null, -32_600, "the request line is over 1 MiB");
    let _ = wire.send(&reply);
    let _ = reader.get_ref().shutdown(Shutdown::Write);
    if !line.ends_with(b"\n") {
        drain_line(reader);
    }
}

/// Drain only through the newline, one more MiB, or 500 ms.
fn drain_line(reader: &mut BufReader<UnixStream>) {
    let until = Instant::now().checked_add(Duration::from_millis(500));
    let mut left = MAX_LINE;

    while left > 0 {
        let timeout = until.map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(Instant::now())
        });
        if timeout.is_zero() {
            break;
        }

        let _ = reader.get_ref().set_read_timeout(Some(timeout));
        let Ok(bytes) = reader.fill_buf() else { break };
        if bytes.is_empty() {
            break;
        }

        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let count = newline
            .map_or(bytes.len(), |position| position.saturating_add(1))
            .min(left);
        reader.consume(count);
        left = left.saturating_sub(count);
        if newline.is_some() {
            break;
        }
    }
}

struct Wire {
    writer: Mutex<UnixStream>,
    dead: AtomicBool,
}

impl Wire {
    fn send(&self, message: &Value) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if self.dead.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }

        let line = format!("{message}\n");
        let result = write_message(&mut writer, line.as_bytes());

        if result.is_err() {
            self.dead.store(true, Ordering::SeqCst);
            let _ = writer.shutdown(Shutdown::Both);
        }

        result
    }
}

/// Partial writes share one message deadline, so trickling readers cannot
/// extend a message's lifetime indefinitely.
fn write_message(writer: &mut UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    let until = Instant::now().checked_add(WRITE);

    while !bytes.is_empty() {
        let left = until.map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(Instant::now())
        });
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }

        writer.set_write_timeout(Some(left))?;
        let written = writer.write(bytes)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = bytes.get(written..).unwrap_or_default();
    }

    Ok(())
}

#[derive(Default)]
struct Calls {
    open: Mutex<Vec<(Value, Cancel, bool)>>,
}

/// A call's place among a connection's open calls, given back when it drops:
/// a call thread that unwinds still frees its place.
struct Slot<'a> {
    calls: &'a Calls,
    id: Value,
    cancel: Cancel,
}

impl Slot<'_> {
    /// The call's ID, which its reply carries.
    const fn id(&self) -> &Value {
        &self.id
    }

    /// The call's cancellation, shared with `$/cancelRequest` and the
    /// connection's end.
    const fn cancel(&self) -> &Cancel {
        &self.cancel
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.calls.free(&self.id);
    }
}

impl Calls {
    fn is_empty(&self) -> bool {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    fn room(&self, id: &Value, navigate: bool) -> Result<(), Value> {
        let (taken, count, navigations) = {
            let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            (
                open.iter().any(|(open, _, _)| open == id),
                open.len(),
                open.iter().filter(|(_, _, navigation)| *navigation).count(),
            )
        };

        if taken {
            return Err(rpc::duplicate(id));
        }
        if count >= MAX_CALLS {
            return Err(busy_reply(
                id,
                &format!("the daemon is busy: this connection has {MAX_CALLS} calls open"),
            ));
        }
        if navigate && navigations >= MAX_NAVIGATIONS {
            return Err(busy_reply(
                id,
                &format!(
                    "the daemon is busy: this connection carries {MAX_NAVIGATIONS} navigations already"
                ),
            ));
        }

        Ok(())
    }

    /// Opens a place for a call with `id`, giving it back and cancelling it
    /// when the slot drops.
    fn open(&self, id: Value, navigate: bool) -> Slot<'_> {
        let source = if navigate {
            Source::Integration
        } else {
            Source::Harness
        };
        let cancel = Cancel::new(source);
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id.clone(), cancel.clone(), navigate));
        Slot {
            calls: self,
            id,
            cancel,
        }
    }

    /// Removes the call with `id` and cancels it, so any worker still using
    /// its cancellation stops.
    fn free(&self, id: &Value) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, cancel, _) in open.iter().filter(|(open, _, _)| open == id) {
            cancel.cancel();
        }
        open.retain(|(open, _, _)| open != id);
    }

    fn cancel(&self, id: Option<&Value>) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, cancel, _) in open.iter().filter(|(open, _, _)| Some(open) == id) {
            cancel.cancel();
        }
    }

    fn cancel_all(&self) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        for (_, cancel, _) in open.iter() {
            cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use chauffeur_runtime::{Cancel, Source};
    use serde_json::{Value, json};

    use super::{
        ACCEPT_RETRY, Calls, MAX_CALLS, MAX_NAVIGATIONS, Next, ReadOutcome, Wire, accept_next,
        answer_and_reply, busy_error, classify, next, serve_connection,
    };
    use crate::route::Router;
    use crate::{Paths, rpc};

    /// An error that lasts, such as `EMFILE`, fails every accept at once;
    /// waiting between them keeps the loop from spinning a core.
    #[test]
    fn each_failed_accept_waits_before_the_next() {
        let mut calls = 0_u32;
        let started = Instant::now();

        let accepted = accept_next(
            || {
                calls = calls.saturating_add(1);
                if calls <= 3 {
                    Err(io::Error::other("too many open files"))
                } else {
                    Ok(calls)
                }
            },
            |_| {},
        );

        assert_eq!(accepted, 4, "accepted on the fourth try");
        assert!(
            started.elapsed() >= ACCEPT_RETRY.saturating_mul(3),
            "{:?} for three failed accepts",
            started.elapsed()
        );
    }

    /// The first failure of a spell is logged once, however long the spell
    /// lasts: a lasting fault must not flood the log.
    #[test]
    fn a_spell_of_failed_accepts_is_logged_once() {
        let mut calls = 0_u32;
        let mut logged = Vec::new();

        let accepted = accept_next(
            || {
                calls = calls.saturating_add(1);
                if calls <= 3 {
                    Err(io::Error::other("too many open files"))
                } else {
                    Ok(calls)
                }
            },
            |error| logged.push(error.to_string()),
        );

        assert_eq!(accepted, 4);
        assert_eq!(logged, ["too many open files"]);
    }

    #[test]
    fn an_accept_that_succeeds_is_not_tried_again() {
        let mut calls = 0_u32;

        let accepted = accept_next(
            || {
                calls = calls.saturating_add(1);
                io::Result::Ok("connection")
            },
            |_| {},
        );

        assert_eq!(accepted, "connection");
        assert_eq!(calls, 1);
    }

    /// A router over `models`, the user's models file, and the repository's
    /// shipped rules and safety patterns.
    fn fixture(name: &str, models: &str) -> Router {
        let root =
            std::env::temp_dir().join(format!("chauffeur-socket-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/models.json"), models).unwrap();
        let paths = Paths {
            name: "chauffeur-test".into(),
            socket: root.join("run/daemon.sock"),
            config: root.join("config"),
            distribution: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../distribution"),
            state: root.join("state"),
            models: Some(root.join("config/models.json")),
        };
        let loaded = crate::state::load(&paths, None).unwrap();

        Router::new(
            loaded,
            crate::audit::Audit::new(&paths.state, crate::audit::MAX_BYTES),
        )
    }

    /// The endpoint of a model that accepts requests and never answers, so a
    /// call must give up at its deadline rather than wait for its model.
    fn silent_model() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming().filter_map(Result::ok) {
                // Held open: the judge reads no answer from it.
                thread::sleep(Duration::from_secs(60));
                drop(stream);
            }
        });
        endpoint
    }

    /// A daemon with no capacity, whether for another connection or for a
    /// call thread, says it is busy and why.
    #[test]
    fn a_busy_error_names_the_reason_and_the_message() {
        let error =
            busy_error("the daemon is busy: cannot start a call thread: too many open files");

        assert_eq!(error["code"], -32_000);
        assert_eq!(error["data"]["reason"], "busy");
        assert_eq!(
            error["message"],
            "the daemon is busy: cannot start a call thread: too many open files"
        );
    }

    /// With nothing sent, the connection ends at the idle timeout, so a
    /// silent client cannot hold a thread forever.
    #[test]
    fn an_idle_connection_ends_after_its_idle_timeout() {
        let router = Arc::new(fixture("idle", r#"{ "models": {}, "tiers": {} }"#));
        let (server, _client) = UnixStream::pair().unwrap();
        let (ended, closed) = mpsc::channel();

        thread::spawn(move || {
            serve_connection(&router, server, Duration::from_millis(200));
            let _ = ended.send(());
        });

        let waited = closed.recv_timeout(Duration::from_secs(2));

        assert!(
            waited.is_ok(),
            "an idle connection ends at its idle timeout"
        );
    }

    /// A call that outlives the idle timeout keeps its connection: only an
    /// empty connection is timed out.
    #[test]
    fn an_open_call_keeps_the_connection_past_the_idle_timeout() {
        let endpoint = silent_model();
        let models = json!({
            "models": {
                "slow": { "format": "systemone", "endpoint": endpoint, "model": "jev-latest",
                          "key": { "env": "HOME" }, "timeout_ms": 5000 }
            },
            "tiers": { "system-one": ["slow"] }
        })
        .to_string();
        let router = Arc::new(fixture("open-call", &models));
        let (server, mut client) = UnixStream::pair().unwrap();
        let reading = client.try_clone().unwrap();
        let limit = Some(Duration::from_secs(10));
        client.set_write_timeout(limit).unwrap();
        reading.set_read_timeout(limit).unwrap();
        thread::spawn(move || serve_connection(&router, server, Duration::from_millis(200)));

        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "chauffeur.navigate",
            "params": {
                "protocol": 1, "deadline_ms": 1500,
                "judges": { "slow": { "tier": "system-one", "model": "slow" } },
                "strategy": [{ "id": "done", "judge": "slow",
                               "ask": { "outcomes": ["yes", "no"], "evidence": ["log"],
                                        "instructions": "Is it finished?" } }],
                "evidence": { "log": "x" }
            }
        });
        writeln!(client, "{request}").unwrap();

        let mut line = String::new();
        BufReader::new(reading).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        let reason = reply
            .get("error")
            .and_then(|error| error.get("data"))
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str);

        assert!(
            reply.get("result").is_some() || reason == Some("deadline"),
            "the call outlives the idle timeout: {reply}"
        );
        assert_ne!(reason, Some("cancelled"), "{reply}");
    }

    /// A connection that carried a call is idle again once the call is done:
    /// going quiet after one call cannot hold its thread for ever.
    #[test]
    fn a_connection_that_carried_a_call_ends_at_its_idle_timeout() {
        let router = Arc::new(fixture("after-call", r#"{ "models": {}, "tiers": {} }"#));
        let (server, mut client) = UnixStream::pair().unwrap();
        let limit = Some(Duration::from_secs(2));
        client.set_read_timeout(limit).unwrap();
        client.set_write_timeout(limit).unwrap();
        let mut reading = BufReader::new(client.try_clone().unwrap());

        thread::spawn(move || serve_connection(&router, server, Duration::from_millis(200)));

        writeln!(
            client,
            r#"{{ "jsonrpc": "2.0", "id": 1, "method": "chauffeur.status" }}"#
        )
        .unwrap();
        let mut reply = String::new();
        reading.read_line(&mut reply).unwrap();
        assert!(reply.contains("\"result\""), "{reply}");

        let mut ended = String::new();
        let waited = reading.read_line(&mut ended);

        assert!(
            matches!(waited, Ok(0)),
            "the connection is still open after its call: {waited:?}"
        );
    }

    /// A call's place and its cancellation are given back when its slot
    /// drops.
    #[test]
    fn dropping_a_call_slot_frees_the_call_and_cancels_it() {
        let calls = Calls::default();
        let slot = calls.open(json!(1), false);
        let cancel = slot.cancel().clone();

        assert!(!calls.is_empty(), "the call is open");
        assert!(!cancel.is_cancelled());

        drop(slot);

        assert!(calls.is_empty(), "the slot gave its place back");
        assert!(cancel.is_cancelled(), "the slot cancelled its call");
    }

    /// A call thread that unwinds still gives its place back: its slot drops
    /// as the thread unwinds.
    #[test]
    fn a_call_thread_that_unwinds_gives_its_place_back() {
        let calls = Calls::default();
        let slot = calls.open(json!(1), false);
        let cancel = slot.cancel().clone();

        thread::scope(|scope| {
            let unwound = scope.spawn(move || {
                let _slot = slot;
                panic!("the call unwound");
            });
            assert!(unwound.join().is_err(), "the call thread unwound");
        });

        assert!(calls.is_empty(), "the slot gave its place back");
        assert!(cancel.is_cancelled(), "the slot cancelled its call");
    }

    /// A call that panics is answered as an internal error instead of
    /// leaving its caller without a reply, and its place is free afterwards.
    #[test]
    fn a_call_that_panics_is_answered_as_internal_error() {
        let calls = Calls::default();
        let slot = calls.open(json!("panicky"), false);
        let cancel = slot.cancel().clone();
        let (server, client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let wire = Wire {
            writer: Mutex::new(server),
            dead: AtomicBool::new(false),
        };

        answer_and_reply(&calls, &wire, slot, |_| panic!("the answer panicked"));

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(reply["jsonrpc"], "2.0", "{reply}");
        assert_eq!(reply["id"], "panicky", "{reply}");
        assert_eq!(reply["error"]["code"], -32_603, "{reply}");
        assert_eq!(reply["error"]["message"], "internal error", "{reply}");
        assert!(calls.is_empty(), "the call's place is free");
        assert!(cancel.is_cancelled(), "the call is cancelled as it frees");
    }

    /// A call whose cancel was set and then panicked is answered as
    /// cancelled, with the reason its caller named, not as an internal
    /// error.
    #[test]
    fn a_panicking_call_that_was_cancelled_is_answered_as_cancelled() {
        let calls = Calls::default();
        let slot = calls.open(json!("stopped"), false);
        let cancel = slot.cancel().clone();
        let (server, client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let wire = Wire {
            writer: Mutex::new(server),
            dead: AtomicBool::new(false),
        };

        cancel.cancel();
        answer_and_reply(&calls, &wire, slot, |_| panic!("the answer panicked"));

        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(reply["jsonrpc"], "2.0", "{reply}");
        assert_eq!(reply["id"], "stopped", "{reply}");
        assert_eq!(reply["error"]["code"], -32_800, "{reply}");
        assert_eq!(
            reply["error"]["message"], "the caller cancelled the request",
            "{reply}"
        );
        assert_eq!(reply["error"]["data"]["reason"], "cancelled", "{reply}");
        assert!(calls.is_empty(), "the call's place is free");
    }

    /// A line cut short by the read timeout is kept and completed by the
    /// next read, and the timeout comes back apart from an end.
    #[test]
    fn a_partial_line_survives_the_idle_timeout() {
        let (server, mut client) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let wire = Wire {
            writer: Mutex::new(server.try_clone().unwrap()),
            dead: AtomicBool::new(false),
        };
        let mut reader = BufReader::new(server);
        let mut line = Vec::new();

        client.write_all(b"{ \"jsonrpc\": \"2.0\",").unwrap();
        assert!(matches!(next(&mut reader, &mut line, &wire), Next::Idle));
        assert!(!line.is_empty(), "the partial line is kept");

        writeln!(client, "\"id\": 1, \"method\": \"chauffeur.status\" }}").unwrap();
        let Next::Request(request) = next(&mut reader, &mut line, &wire) else {
            panic!("the completed line is read as a request");
        };

        assert_eq!(request.method, "chauffeur.status");
        assert_eq!(request.id, Some(json!(1)));
        assert!(line.is_empty(), "the line is cleared once it is read");
    }

    /// The read timeout is told apart from a failure: a reset or a broken
    /// pipe ends the connection rather than idling it.
    #[test]
    fn a_read_timeout_is_told_apart_from_a_failed_read() {
        assert!(matches!(classify(Ok(0)), ReadOutcome::Line(0)));
        assert!(matches!(classify(Ok(7)), ReadOutcome::Line(7)));

        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            assert!(
                matches!(classify(Err(kind.into())), ReadOutcome::TimedOut),
                "{kind:?} is the read timeout"
            );
        }

        for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::BrokenPipe] {
            let ReadOutcome::Failed(error) = classify(Err(kind.into())) else {
                panic!("{kind:?} is a failure, not the read timeout");
            };
            assert_eq!(error.kind(), kind);
        }
    }

    /// A reader whose every read fails with the error it was made with.
    struct Failing {
        kind: io::ErrorKind,
    }

    impl Read for Failing {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(self.kind.into())
        }
    }

    impl BufRead for Failing {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(self.kind.into())
        }

        fn consume(&mut self, _amount: usize) {}
    }

    /// A wire for a read that fails before anything is answered.
    fn unwritten() -> Wire {
        let (writer, _reader) = UnixStream::pair().unwrap();
        Wire {
            writer: Mutex::new(writer),
            dead: AtomicBool::new(false),
        }
    }

    /// A read that failed ends the connection as a failure carrying its
    /// error, not as the idle timeout or an end.
    #[test]
    fn a_failed_read_ends_the_connection_as_a_failure() {
        let wire = unwritten();
        let mut line = Vec::new();

        let Next::Failed(error) = next(
            &mut Failing {
                kind: io::ErrorKind::ConnectionReset,
            },
            &mut line,
            &wire,
        ) else {
            panic!("a reset is a failure, not the idle timeout");
        };

        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }

    /// A read that would block is the read timeout: the caller reads again,
    /// keeping what it has, rather than ending the connection.
    #[test]
    fn a_read_that_would_block_is_the_idle_timeout() {
        let wire = unwritten();
        let mut line = Vec::new();

        let outcome = next(
            &mut Failing {
                kind: io::ErrorKind::WouldBlock,
            },
            &mut line,
            &wire,
        );

        assert!(matches!(outcome, Next::Idle), "WouldBlock is the timeout");
    }

    /// Each cap refuses as busy and says which one it hit, so a caller knows
    /// what to wait for.
    #[test]
    fn a_cap_refuses_as_busy_and_names_its_limit() {
        let calls = Calls::default();
        let navigations: Vec<_> = (0..MAX_NAVIGATIONS)
            .map(|index| calls.open(json!(index), true))
            .collect();

        let refused = calls.room(&json!("fifth"), true).unwrap_err();

        assert_eq!(refused["error"]["code"], -32_000, "{refused}");
        assert_eq!(
            refused["error"]["message"],
            "the daemon is busy: this connection carries 4 navigations already"
        );

        let others: Vec<_> = (MAX_NAVIGATIONS..MAX_CALLS)
            .map(|index| calls.open(json!(index), false))
            .collect();

        let refused = calls.room(&json!("another"), false).unwrap_err();

        assert_eq!(refused["error"]["code"], -32_000, "{refused}");
        assert_eq!(
            refused["error"]["message"],
            "the daemon is busy: this connection has 16 calls open"
        );

        drop(navigations);
        drop(others);
    }

    /// A call cancelled before it is answered is refused as cancelled,
    /// whatever its method, not only a navigation.
    #[test]
    fn a_cancelled_call_is_answered_as_cancelled() {
        let router = fixture("cancelled", r#"{ "models": {}, "tiers": {} }"#);
        let request =
            rpc::parse(r#"{"jsonrpc": "2.0", "id": 1, "method": "chauffeur.status"}"#).unwrap();
        let call = rpc::prepare(&router, request).unwrap();
        let cancel = Cancel::new(Source::Harness);
        cancel.cancel();

        let (reply, _permit) = call.answer(&router, &cancel, &|_| {});
        let reply = reply.unwrap();

        assert_eq!(reply["error"]["code"], -32_800, "{reply}");
        assert_eq!(reply["error"]["data"]["reason"], "cancelled", "{reply}");
    }
}
