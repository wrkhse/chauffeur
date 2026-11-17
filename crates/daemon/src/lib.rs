//! The Chauffeur daemon: loads models, redaction and rules, reloads them when
//! their files change, answers JSON-RPC 2.0 lines on a Unix socket in a
//! folder only the user can open, and writes every run to the audit log.
//!
//! | Method | Params | Result |
//! |---|---|---|
//! | `chauffeur.emit` | `event`, `deadline_ms` | `actions`: what each hooked rule returned |
//! | `chauffeur.navigate` | A request: `protocol`, `on` or `strategy`, `evidence`, `deadline_ms` | `intents`, or `verdict`; `chauffeur.event` notifications before it |
//! | `chauffeur.status` | none | Version, protocol, loaded rules, refused rule files |
//! | `$/cancelRequest` | `id` | Nothing: the call with that ID is answered as `cancelled` |
//!
//! A connection has at most 16 calls open, at most 4 of them navigations: a
//! fifth is refused as busy until one finishes.
//!
//! A request with no `id` is a notification: apart from `$/cancelRequest`,
//! which cancels the call it names, it runs nothing, neither a method nor a
//! reply.

pub mod audit;
mod paths;
mod route;
mod rpc;
mod socket;
mod state;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::event::{MetadataKind, ModifyKind};
use notify::{EventKind, PollWatcher, RecursiveMode, Watcher};

pub use paths::{Paths, private_folder};
pub use route::Router;
pub use socket::{MAX_CALLS, MAX_CONNECTIONS, MAX_LINE, MAX_NAVIGATIONS};

/// How long file changes settle before a reload.
const SETTLE: Duration = Duration::from_millis(200);

/// The longest file changes settle: changes that keep coming still reload
/// this long after the first.
const SETTLE_AT_MOST: Duration = Duration::from_secs(2);

/// How often configuration is checked when the platform cannot report
/// changes, such as in a sandbox that refuses `FSEvents`.
const POLL: Duration = Duration::from_secs(2);

/// Why the daemon stopped or would not start.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Another daemon answers on the socket.
    #[error("a daemon is already running on {0}")]
    Running(PathBuf),
    /// The socket's folder is not private to the user.
    #[error("the socket folder {path} {reason}")]
    SocketFolder {
        /// The folder.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The socket could not be bound.
    #[error("cannot bind {path}: {source}")]
    Socket {
        /// The socket.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The thread that reloads configuration could not start.
    #[error("cannot start the reload thread: {0}")]
    Thread(std::io::Error),
    /// Models or redaction could not load.
    #[error("{0}")]
    Config(String),
    /// Changes to configuration cannot be watched.
    #[error("cannot watch configuration: {0}")]
    Watch(#[from] notify::Error),
}

/// Loads everything for `paths`, binds the socket and serves for as long as
/// the process runs. Configuration that changes is reloaded; a change that
/// does not load keeps what was loaded before.
///
/// # Errors
///
/// A [`DaemonError`] if the daemon cannot start, the only way it returns.
pub fn run(paths: &Paths) -> Result<!, DaemonError> {
    // Watch before the first load, so a change made while it loads, or just
    // after the socket opens, still reloads.
    let (send, receive) = mpsc::channel();
    // Bound until the process ends: dropping it stops watching.
    let _watcher = watch(&[&paths.distribution, &paths.config], &send)?;
    let audit = audit::Audit::new(&paths.state, audit::MAX_BYTES);
    let router = Arc::new(Router::new(state::load(paths, None)?, audit));
    let listener = socket::bind(&paths.socket)?;
    let (reloader, reload_paths) = (Arc::clone(&router), paths.clone());
    thread::Builder::new()
        .name("reload".into())
        .spawn(move || reload(&reloader, &reload_paths, &receive))
        .map_err(DaemonError::Thread)?;
    socket::serve(&listener, &router)
}

/// Watches `folders` with the platform's watcher, or by polling when that
/// watcher cannot start. Each change that may alter what loads sends one
/// message.
fn watch(folders: &[&Path], send: &mpsc::Sender<()>) -> Result<Box<dyn Watcher>, DaemonError> {
    match start(notify::recommended_watcher(forward(send.clone())), folders) {
        Ok(watcher) => Ok(watcher),
        Err(error) => {
            eprintln!(
                "chauffeur: checking configuration for changes every {} s: {error}",
                POLL.as_secs()
            );
            let config = notify::Config::default().with_poll_interval(POLL);
            Ok(start(
                PollWatcher::new(forward(send.clone()), config),
                folders,
            )?)
        }
    }
}

/// Sends one message for each event that may change what loads.
fn forward(send: mpsc::Sender<()>) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    move |event| {
        if event.is_ok_and(|event| changes(event.kind)) {
            let _ = send.send(());
        }
    }
}

/// Whether an event of `kind` may change what loads. Opening, reading and
/// closing a file are not changes, nor are its permissions and other
/// metadata: on Linux the daemon's own reads report `Access`, and reloading
/// on them never settles. The poll watcher reports an edit as a newer write
/// time; a kind the platform cannot tell, or a notice that events were lost,
/// may hide a change.
const fn changes(kind: EventKind) -> bool {
    match kind {
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)) => true,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Any
        | EventKind::Other => true,
    }
}

fn start<W: Watcher + 'static>(
    watcher: notify::Result<W>,
    folders: &[&Path],
) -> notify::Result<Box<dyn Watcher>> {
    let mut watcher = watcher?;
    for folder in folders.iter().filter(|folder| folder.is_dir()) {
        watcher.watch(folder, RecursiveMode::Recursive)?;
    }
    Ok(Box::new(watcher))
}

fn reload(router: &Router, paths: &Paths, changes: &mpsc::Receiver<()>) {
    while changes.recv().is_ok() {
        settle(changes, SETTLE, SETTLE_AT_MOST);
        match state::load(paths, router.loaded().as_deref()) {
            Ok(loaded) => router.replace(loaded),
            Err(error) => eprintln!("chauffeur: kept the loaded configuration: {error}"),
        }
    }
}

/// Takes the changes that follow until none has come for `quiet`, or
/// `longest` has passed, so a stream of changes still reloads.
fn settle(changes: &mpsc::Receiver<()>, quiet: Duration, longest: Duration) {
    let started = Instant::now();
    loop {
        let left = longest.saturating_sub(started.elapsed());
        if left.is_zero() || changes.recv_timeout(quiet.min(left)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
    use std::thread;
    use std::time::{Duration, Instant};

    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify::{Config, EventKind, PollWatcher};

    use super::{DaemonError, changes, forward, settle, start};

    #[test]
    fn a_reload_thread_that_cannot_start_says_so() {
        let error = DaemonError::Thread(io::Error::other("no thread left"));

        assert_eq!(
            error.to_string(),
            "cannot start the reload thread: no thread left"
        );
    }

    #[test]
    fn opening_reading_and_metadata_are_not_changes() {
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ] {
            assert!(!changes(kind), "{kind:?}");
        }
    }

    #[test]
    fn creating_writing_renaming_and_removing_are_changes() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Remove(RemoveKind::File),
            // How the poll watcher reports an edit.
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Any,
            // A notice that events were lost.
            EventKind::Other,
        ] {
            assert!(changes(kind), "{kind:?}");
        }
    }

    #[test]
    fn settling_takes_the_changes_that_follow_within_the_quiet_time() {
        let (send, receive) = mpsc::channel();
        let sending = thread::spawn(move || {
            for _ in 0..3 {
                send.send(()).unwrap();
                thread::sleep(Duration::from_millis(100));
            }
            // Still connected while settling waits out its quiet time.
            thread::sleep(Duration::from_secs(1));
        });
        let started = Instant::now();

        settle(&receive, Duration::from_millis(300), Duration::from_secs(5));

        let took = started.elapsed();
        assert!(took >= Duration::from_millis(400), "{took:?}");
        // It returns once the quiet time passes, well before its bound.
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
        sending.join().unwrap();
    }

    #[test]
    fn settling_ends_at_its_bound_while_changes_keep_coming() {
        let (send, receive) = mpsc::channel();
        let sending = thread::spawn(move || {
            // Stops after 3 s even if settling never would.
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(3) && send.send(()).is_ok() {
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();

        settle(
            &receive,
            Duration::from_millis(200),
            Duration::from_millis(500),
        );

        let took = started.elapsed();
        drop(receive);
        sending.join().unwrap();
        assert!(took < Duration::from_millis(1500), "{took:?}");
    }

    /// A fresh folder holding `rules/a.json`.
    fn folder(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("chauffeur-watch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("rules")).unwrap();
        fs::write(root.join("rules/a.json"), "{}").unwrap();
        root
    }

    /// Takes changes until none has come for half a second, for at most 5 s.
    fn quiet(receive: &Receiver<()>) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5)
            && receive.recv_timeout(Duration::from_millis(500)).is_ok()
        {}
    }

    /// Reading the rules is not a change; creating, editing, renaming and
    /// removing one each is.
    fn only_changes_reload(root: &Path, receive: &Receiver<()>) {
        let rules = root.join("rules");
        quiet(receive);

        fs::read_to_string(rules.join("a.json")).unwrap();
        assert!(fs::read_dir(&rules).unwrap().count() > 0);
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Timeout),
            "reading was taken for a change"
        );

        let steps: [(&str, &dyn Fn() -> io::Result<()>); 4] = [
            ("creating", &|| fs::write(rules.join("b.json"), "{}")),
            ("editing", &|| {
                fs::write(rules.join("b.json"), r#"{ "edited": true }"#)
            }),
            ("renaming", &|| {
                fs::rename(rules.join("b.json"), rules.join("c.json"))
            }),
            ("removing", &|| fs::remove_file(rules.join("c.json"))),
        ];
        for (step, change) in steps {
            // The poll watcher compares write times to the second.
            thread::sleep(Duration::from_millis(1100));
            change().unwrap();
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(5)),
                Ok(()),
                "{step} did not reload"
            );
            quiet(receive);
        }
    }

    #[test]
    fn the_platforms_watcher_reloads_on_changes_and_not_on_reads() {
        let root = folder("platform");
        let (send, receive) = mpsc::channel();
        let _watcher = start(notify::recommended_watcher(forward(send)), &[&root]).unwrap();

        only_changes_reload(&root, &receive);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn polling_reloads_on_changes_and_not_on_reads() {
        let root = folder("poll");
        let (send, receive) = mpsc::channel();
        let config = Config::default().with_poll_interval(Duration::from_millis(100));
        let _watcher = start(PollWatcher::new(forward(send), config), &[&root]).unwrap();

        only_changes_reload(&root, &receive);

        let _ = fs::remove_dir_all(&root);
    }
}
