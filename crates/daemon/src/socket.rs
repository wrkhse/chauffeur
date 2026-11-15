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
        let stream = accept_next(|| listener.accept().map(|(stream, _)| stream));
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
fn accept_next<S>(mut accept: impl FnMut() -> io::Result<S>) -> S {
    let mut failed = false;

    loop {
        match accept() {
            Ok(stream) => return stream,
            Err(error) if !failed => {
                eprintln!("chauffeur: cannot accept a connection: {error}; retrying");
                failed = true;
            }
            Err(_) => {}
        }

        thread::sleep(ACCEPT_RETRY);
    }
}

fn busy(mut stream: UnixStream) {
    let reply = json!({ "jsonrpc": "2.0", "id": null, "error": Rejection::Busy.to_error() });

    if stream.set_write_timeout(Some(BUSY_WRITE)).is_ok() {
        let _ = writeln!(stream, "{reply}");
    }
}

/// Reads and admits on this thread; each admitted call gets a fallible
/// scoped thread. A notification runs only for emit, without a reply.
fn connection(router: &Router, stream: UnixStream) {
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
                .set_read_timeout(calls.is_empty().then_some(IDLE));
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
                    let error =
                        rpc::error(&id, -32_000, &format!("cannot start call thread: {error}"));
                    let _ = wire.send(&error);
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
    use std::io;
    use std::time::Instant;

    use super::{ACCEPT_RETRY, accept_next};

    /// An error that lasts, such as `EMFILE`, fails every accept at once;
    /// waiting between them keeps the loop from spinning a core.
    #[test]
    fn each_failed_accept_waits_before_the_next() {
        let mut calls = 0_u32;
        let started = Instant::now();

        let accepted = accept_next(|| {
            calls = calls.saturating_add(1);
            if calls <= 3 {
                Err(io::Error::other("too many open files"))
            } else {
                Ok(calls)
            }
        });

        assert_eq!(accepted, 4, "accepted on the fourth try");
        assert!(
            started.elapsed() >= ACCEPT_RETRY.saturating_mul(3),
            "{:?} for three failed accepts",
            started.elapsed()
        );
    }

    #[test]
    fn an_accept_that_succeeds_is_not_tried_again() {
        let mut calls = 0_u32;

        let accepted = accept_next(|| {
            calls = calls.saturating_add(1);
            io::Result::Ok("connection")
        });

        assert_eq!(accepted, "connection");
        assert_eq!(calls, 1);
    }
}
