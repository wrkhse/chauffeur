mod cooldown;
mod log_entry;
mod navigate;

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chauffeur_judges::Answer;
use chauffeur_protocol::{Event, Rule, Then};
use chauffeur_runtime::{
    Admission, Busy, Cancel, Capacity, Job, Permit, Pool, Source, run_strategy,
};
use cooldown::Cooldowns;
use serde_json::{Map, Value, json};

use crate::audit::Audit;
use crate::state::Loaded;

/// How many rule runs are kept waiting between calls: enough for every rule
/// hooked by one event to start at once.
const RULES_KEPT: usize = 8;

/// The most rule runs alive at once. Each takes a runtime worker of its own,
/// so this is the daemon's whole capacity.
const RULES_ALIVE: usize = 32;

/// How often the dispatcher looks for a finished run while every worker is
/// busy. A worker that has just finished may not be waiting for a job yet, so
/// a look that finds nothing does not mean another run is still out.
const LOOK: Duration = Duration::from_millis(10);

/// How long the caller waits past the deadline for the runs already started:
/// a run stops when the deadline passes, and reports what it did just after.
const GRACE: Duration = Duration::from_millis(50);

/// Routes events and navigations to the rules that hook them, admits each
/// within its source's share, runs the rules on a pool of workers, and writes
/// what they did to the audit log.
pub struct Router {
    loaded: RwLock<Arc<Loaded>>,
    cooldowns: Arc<Cooldowns>,
    admission: Admission,
    rules: Pool,
    audit: Arc<Audit>,
}

/// One rule's run for an event or a navigation: what it was given, owned so
/// the worker that runs it takes its own copy.
#[derive(Clone)]
struct Ask {
    event: String,
    session: String,
    evidence: Arc<Vec<(String, String)>>,
    deadline: Instant,
    cancel: Cancel,
}

/// One rule's run: its log entry, what it returned, and its run, if it ran.
struct RuleRun {
    entry: Value,
    then: Option<Then>,
    run: Option<chauffeur_core::Run<Answer>>,
}

impl Router {
    /// A router over `loaded`, writing each run to `audit`.
    #[must_use]
    pub fn new(loaded: Loaded, audit: Audit) -> Self {
        Self {
            loaded: RwLock::new(Arc::new(loaded)),
            cooldowns: Arc::new(Cooldowns::new()),
            admission: Admission::new(Capacity::DAEMON),
            rules: Pool::new(RULES_KEPT, RULES_ALIVE),
            audit: Arc::new(audit),
        }
    }

    /// Replaces the loaded rules, models and redaction; the next event uses them.
    pub fn replace(&self, loaded: Loaded) {
        if let Ok(mut current) = self.loaded.write() {
            *current = Arc::new(loaded);
        }
    }

    /// The current rules, models and redaction.
    #[must_use]
    pub fn loaded(&self) -> Option<Arc<Loaded>> {
        self.loaded.read().ok().map(|loaded| Arc::clone(&loaded))
    }

    /// A place for one call from `source`. The call gives its own copies
    /// back as it is answered, before the reply is written; a run still
    /// working keeps its own copy, so the place is held until the last one
    /// finishes.
    ///
    /// # Errors
    ///
    /// [`Busy`] while the source's share or the capacity is in use.
    pub fn admit(&self, source: Source) -> Result<Permit, Busy> {
        self.admission.admit(source)
    }

    /// Runs every rule that hooks `event` on the router's pool, each within
    /// `deadline` or until `cancel`, and returns what each rule returned.
    pub fn emit(&self, event: &Event, deadline: Duration, cancel: &Cancel) -> Vec<Value> {
        let Some(loaded) = self.loaded() else {
            return Vec::new();
        };
        let Some(deadline) = Instant::now().checked_add(deadline) else {
            return Vec::new();
        };
        let hooked: Vec<Arc<Rule>> = loaded
            .rules
            .iter()
            .filter(|rule| rule.hooks(event))
            .cloned()
            .collect();
        let ask = Ask {
            event: event.name().to_owned(),
            session: event.session().unwrap_or_default().to_owned(),
            evidence: Arc::new(event.evidence()),
            deadline,
            cancel: cancel.clone(),
        };
        self.run_all(&loaded, &hooked, &ask, &|_, _| {})
            .into_iter()
            .filter_map(|(rule, then)| {
                then.map(|then| {
                    json!({ "rule": rule.id(), "action": then.name(), "payload": then.payload() })
                })
            })
            .collect()
    }

    /// Runs `hooked` on the router's pool, calling `ran` as each result
    /// arrives, and returns what each returned, in the order given. Each run
    /// is written to the log, in that same order.
    fn run_all(
        &self,
        loaded: &Arc<Loaded>,
        hooked: &[Arc<Rule>],
        ask: &Ask,
        ran: &(dyn Fn(&Rule, &RuleRun) + Sync),
    ) -> Vec<(Arc<Rule>, Option<Then>)> {
        let mut returned: BTreeMap<usize, RuleRun> = BTreeMap::new();
        let jobs = |index: usize, rule: &Arc<Rule>, send: Sender<(usize, Option<RuleRun>)>| {
            let rule = Arc::clone(rule);
            let loaded = Arc::clone(loaded);
            let cooldowns = Arc::clone(&self.cooldowns);
            let ask = ask.clone();
            let job: Job = Box::new(move || {
                // A rule that panics yields nothing, and its worker runs on.
                let done = panic::catch_unwind(AssertUnwindSafe(|| {
                    Self::run_one(&loaded, &cooldowns, &rule, &ask)
                }))
                .ok();
                let _ = send.send((index, done));
            });
            job
        };
        let arrived = |index: usize, done: Option<RuleRun>| {
            let (Some(done), Some(rule)) = (done, hooked.get(index)) else {
                return;
            };
            ran(rule, &done);
            returned.insert(index, done);
        };
        dispatch(&self.rules, ask.deadline, hooked, jobs, arrived);
        // The indexes are the hooks', so a BTreeMap gives the runs in hook
        // order, and the audit reads in the order the rules load.
        returned
            .into_iter()
            .filter_map(|(index, done)| {
                let rule = hooked.get(index)?;
                let RuleRun {
                    entry,
                    then,
                    run: _,
                } = done;
                // A run still returns when the log cannot be written.
                if let Err(error) = self.audit.append(entry) {
                    eprintln!("chauffeur: a run was not written to the audit log: {error}");
                }
                Some((Arc::clone(rule), then))
            })
            .collect()
    }

    /// One rule's run: its log entry, what it returned, and its run.
    fn run_one(
        loaded: &Arc<Loaded>,
        cooldowns: &Cooldowns,
        rule: &Arc<Rule>,
        ask: &Ask,
    ) -> RuleRun {
        let key = (rule.id().to_owned(), ask.session.clone());
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(rule.id()));
        entry.insert("event".into(), json!(ask.event));
        entry.insert("session".into(), json!(ask.session));
        if cooldowns.cooling(&key, rule.cooldown()) {
            entry.insert("cooldown".into(), json!(true));
            return RuleRun {
                entry: Value::Object(entry),
                then: None,
                run: None,
            };
        }
        let started = Instant::now();
        let ran = run_strategy(
            &loaded.context,
            rule,
            ask.evidence.as_slice(),
            ask.deadline,
            &ask.cancel,
        );
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        entry.insert("elapsed_ms".into(), json!(elapsed));
        let returned = ran.then(rule).cloned();
        let run = log_entry::ended(&mut entry, ran);
        // Runs that settle at once in one session may all have passed
        // `cooling`; only the first to fire returns its action.
        let then = returned.and_then(|then| {
            if !cooldowns.try_fire(key, rule.cooldown()) {
                entry.insert("cooldown".into(), json!(true));
                return None;
            }
            entry.insert("returned".into(), json!(then.name()));
            Some(then)
        });
        RuleRun {
            entry: Value::Object(entry),
            then,
            run,
        }
    }
}

/// Runs one job per item on `pool` and gives each result to `arrived` as it
/// comes, keeping no more jobs running than the pool has workers: a job that
/// finds none free looks for a finished one before trying again. Past
/// `deadline` the items left do not run, and the results still out are not
/// waited for.
fn dispatch<T, R>(
    pool: &Pool,
    deadline: Instant,
    items: &[R],
    job: impl Fn(usize, &R, Sender<(usize, T)>) -> Job,
    mut arrived: impl FnMut(usize, T),
) where
    T: Send + 'static,
{
    let (send, receive) = mpsc::channel();
    let mut pending = items.iter().enumerate().peekable();
    let mut outstanding = 0_usize;
    while let Some(&(index, item)) = pending.peek() {
        if pool.execute(job(index, item, send.clone())).is_ok() {
            pending.next();
            outstanding = outstanding.saturating_add(1);
            continue;
        }
        // No worker is free: a run has to finish before this one can start.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if let Some((index, done)) = finished(&receive, left.min(LOOK)) {
            outstanding = outstanding.saturating_sub(1);
            arrived(index, done);
        }
    }
    // The runs stop at `deadline` and report just after it, so the results
    // still out are given one grace before the caller stops waiting.
    let bound = deadline.checked_add(GRACE).unwrap_or(deadline);
    while outstanding > 0 {
        let left = bound.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some((index, done)) = finished(&receive, left) else {
            break;
        };
        outstanding = outstanding.saturating_sub(1);
        arrived(index, done);
    }
}

/// One finished result, if one comes within `wait`.
fn finished<T>(receive: &Receiver<(usize, T)>, wait: Duration) -> Option<(usize, T)> {
    receive.recv_timeout(wait).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use chauffeur_runtime::Pool;

    use super::dispatch;

    /// The dispatcher keeps no more jobs running than the pool has workers:
    /// every one of 16 jobs comes back, on a pool worker, with no more than
    /// two at once. Both workers are kept waiting between jobs, so the pool
    /// starts no others.
    #[test]
    fn a_dispatch_runs_no_more_jobs_at_once_than_the_pool_has_workers() {
        let pool = Pool::new(2, 2);
        let jobs: Vec<usize> = (0..16).collect();
        let running = Arc::new(AtomicUsize::new(0));
        let highest = Arc::new(AtomicUsize::new(0));
        let workers = Arc::new(Mutex::new(HashSet::new()));
        let names = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(Vec::new()));
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);

        dispatch(
            &pool,
            deadline,
            &jobs,
            |_index, job: &usize, send| {
                let (running, highest) = (Arc::clone(&running), Arc::clone(&highest));
                let (workers, names) = (Arc::clone(&workers), Arc::clone(&names));
                let index = *job;
                Box::new(move || {
                    let at_once = running.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                    highest.fetch_max(at_once, Ordering::SeqCst);
                    let thread = thread::current();
                    workers.lock().unwrap().insert(thread.id());
                    names
                        .lock()
                        .unwrap()
                        .push(thread.name().unwrap_or_default().to_owned());
                    thread::sleep(Duration::from_millis(5));
                    running.fetch_sub(1, Ordering::SeqCst);
                    let _ = send.send((index, index));
                })
            },
            |_, value| done.lock().unwrap().push(value),
        );

        let mut returned = done.lock().unwrap().clone();
        assert_eq!(returned.len(), jobs.len(), "every job came back");
        returned.sort_unstable();
        assert_eq!(returned, jobs, "each job came back once");

        let highest = highest.load(Ordering::SeqCst);
        assert!(highest <= 2, "at most two jobs ran at once: {highest}");
        let workers = workers.lock().unwrap().len();
        assert!(workers <= 2, "at most two workers ran jobs: {workers}");
        for name in names.lock().unwrap().iter() {
            assert!(
                name.starts_with("chauffeur worker"),
                "the job ran on a pool worker: {name}"
            );
        }
    }
}
