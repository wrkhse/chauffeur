use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::DropGuard;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_protocol::Rejection;
use chauffeur_runtime::Cancel;
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
    let error = busy_error(&format!(
        "the daemon is busy: it serves {MAX_CONNECTIONS} connections already"
    ));
    let reply = json!({ "jsonrpc": "2.0", "id": null, "error": error });

    if stream.set_write_timeout(Some(BUSY_WRITE)).is_ok() {
        let _ = writeln!(stream, "{reply}");
    }
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
/// scoped thread. A notification runs only for emit, without a reply.
fn connection(router: &Router, stream: UnixStream) {
    serve_connection(router, stream, IDLE);
}

/// Serves one connection, ending it after `idle` with no request and no call
/// open.
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
        let mut notification = 0_u64;

        loop {
            let _ = reader
                .get_ref()
                .set_read_timeout(calls.is_empty().then_some(idle));
            let Some(request) = next(&mut reader, &mut line, &wire) else {
                break;
            };

            if request.method == rpc::CANCEL {
                calls.cancel(request.params.get("id"));
                continue;
            }

            let reply = request.id.is_some();
            if !reply && request.method != "chauffeur.emit" {
                continue;
            }

            notification = notification.saturating_add(1);
            let id = request
                .id
                .clone()
                .unwrap_or_else(|| json!({ "notification": notification }));
            let navigate = request.method == "chauffeur.navigate";
            let prepared = calls
                .room(&id, navigate)
                .and_then(|()| rpc::prepare(router, request));
            let call = match prepared {
                Ok(call) => call,
                Err(refused) => {
                    if reply {
                        let _ = wire.send(&refused);
                    }
                    continue;
                }
            };
            let cancel = calls.open(id.clone(), navigate);
            let (calls, wire) = (&calls, &wire);
            let key = id.clone();
            let started =
                thread::Builder::new()
                    .name("call".into())
                    .spawn_scoped(scope, move || {
                        finish_call(router, calls, wire, &key, call, &cancel);
                    });

            if let Err(error) = started {
                calls.close(&id);
                if reply {
                    let error = busy_error(&format!(
                        "the daemon is busy: cannot start a call thread: {error}"
                    ));
                    let _ = wire.send(&json!({ "jsonrpc": "2.0", "id": &id, "error": error }));
                }
            }
        }

        calls.cancel_all();
    });
}

fn finish_call(
    router: &Router,
    calls: &Calls,
    wire: &Wire,
    key: &Value,
    call: rpc::Call,
    cancel: &Cancel,
) {
    let notify = |message: Value| {
        if wire.send(&message).is_err() {
            calls.cancel_all();
        }
    };
    let (reply, permit) = call.answer(router, cancel, &notify);
    // Late workers retain their copies; settled calls release before reply.
    drop(permit);
    calls.close(key);

    if let Some(reply) = reply
        && wire.send(&reply).is_err()
    {
        calls.cancel_all();
    }
}

/// Answers malformed lines, returning none on EOF, oversized input or I/O.
fn next(
    reader: &mut BufReader<UnixStream>,
    line: &mut Vec<u8>,
    wire: &Wire,
) -> Option<rpc::Request> {
    loop {
        line.clear();
        let limit = u64::try_from(MAX_LINE)
            .unwrap_or(u64::MAX)
            .saturating_add(1);

        match reader.by_ref().take(limit).read_until(b'\n', line) {
            Ok(0) | Err(_) => return None,
            Ok(_) if line.len() > MAX_LINE => {
                let reply = rpc::error(&Value::Null, -32_600, "the request line is over 1 MiB");
                let _ = wire.send(&reply);
                let _ = reader.get_ref().shutdown(Shutdown::Write);
                if !line.ends_with(b"\n") {
                    drain_line(reader);
                }
                return None;
            }
            Ok(_) => {}
        }

        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            continue;
        }

        match rpc::parse(text.trim()) {
            Ok(request) => return Some(request),
            Err(reply) => wire.send(&reply).ok()?,
        }
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
        if count >= MAX_CALLS || (navigate && navigations >= MAX_NAVIGATIONS) {
            return Err(json!({ "jsonrpc": "2.0", "id": id, "error": Rejection::Busy.to_error() }));
        }

        Ok(())
    }

    fn open(&self, id: Value, navigate: bool) -> Cancel {
        let cancel = Cancel::new();
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, cancel.clone(), navigate));
        cancel
    }

    fn close(&self, id: &Value) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(open, _, _)| open != id);
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
    use std::io::{self, BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::{ACCEPT_RETRY, accept_next, busy_error, serve_connection};
    use crate::Paths;
    use crate::route::Router;

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
}
