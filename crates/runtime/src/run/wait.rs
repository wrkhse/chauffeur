//! Waiting for a job on a worker until it finishes, its deadline passes or
//! its caller cancels it. A result that comes after is dropped.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::pool::{ExecuteError, Pool};
use crate::{Permit, Source};

/// How often a waiting caller looks at its cancellation.
const LOOK: Duration = Duration::from_millis(10);

/// A navigation's cancellation, shared by its caller and its runs. Once
/// cancelled it stays cancelled. Its source picks the pool its runs use.
#[derive(Clone, Debug)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    source: Source,
    // Held only for its Drop: the last copy gives the place back.
    _permit: Option<Permit>,
}

impl Cancel {
    /// A cancellation not yet made, for `source`.
    #[must_use]
    pub fn new(source: Source) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            source,
            _permit: None,
        }
    }

    /// Shares this cancellation, retaining the admitted navigation's place
    /// until every worker using it is really free. The result runs for the
    /// permit's source.
    #[must_use]
    pub fn admitted(&self, permit: Permit) -> Self {
        Self {
            flag: Arc::clone(&self.flag),
            source: permit.source(),
            _permit: Some(permit),
        }
    }

    /// The source whose pool its runs use.
    #[must_use]
    pub const fn source(&self) -> Source {
        self.source
    }

    /// Cancels every run that shares it.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether it was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// How waiting for a job ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum Waited<T> {
    /// The job finished in time.
    Done(T),
    /// The deadline passed first.
    Deadline,
    /// The caller cancelled first.
    Cancelled,
    /// Every worker was busy.
    Busy,
    /// The pool could not start the job's worker, keeping the error it gave.
    Failed(io::Error),
    /// The job's worker stopped without a result.
    Stopped,
}

/// Runs `job` on `pool` and waits for it until `deadline` or until
/// `cancel`. The job is given the deadline and the cancellation, to check
/// before each round it starts.
pub fn within<T: Send + 'static>(
    pool: &Pool,
    deadline: Instant,
    cancel: &Cancel,
    job: impl FnOnce(Instant, &Cancel) -> T + Send + 'static,
) -> Waited<T> {
    if cancel.is_cancelled() {
        return Waited::Cancelled;
    }
    if Instant::now() >= deadline {
        return Waited::Deadline;
    }
    let (send, receive) = mpsc::sync_channel(1);
    let shared = cancel.clone();
    // A send after the caller stopped waiting fails, and the result is dropped.
    let started = pool.execute(Box::new(move || {
        let _ = send.send(job(deadline, &shared));
        // Late results do not free admission before the job finishes.
        drop(shared);
    }));
    if let Err(error) = started {
        // A pool that cannot take the job is not asked again: the deadline
        // would only pass while waiting.
        return not_started(error);
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Waited::Deadline;
        }
        let received = receive.recv_timeout(left.min(LOOK));
        // A caller that cancelled has gone; whatever came for it is dropped.
        if cancel.is_cancelled() {
            return Waited::Cancelled;
        }
        match received {
            Ok(done) => return Waited::Done(done),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Waited::Stopped,
        }
    }
}

/// How a job the pool did not take ends the wait.
fn not_started<T>(error: ExecuteError) -> Waited<T> {
    match error {
        ExecuteError::Busy => Waited::Busy,
        ExecuteError::Spawn(error) => Waited::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{ExecuteError, Waited, not_started};

    /// A worker whose thread could not start is not a busy pool: the
    /// refusal keeps the error that stopped it.
    #[test]
    fn a_worker_that_could_not_start_keeps_its_error() {
        let refused = not_started::<()>(ExecuteError::Spawn(io::Error::other("no thread")));

        assert!(matches!(refused, Waited::Failed(error) if error.to_string() == "no thread"));
        assert!(matches!(
            not_started::<()>(ExecuteError::Busy),
            Waited::Busy
        ));
    }
}
