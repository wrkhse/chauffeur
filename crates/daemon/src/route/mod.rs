mod cooldown;
mod log_entry;
mod navigate;

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
    /// How the run ended without settling, for a `step_settled` line:
    /// `deadline`, `cancelled`, `busy`, `stopped` or `refused`. None when it
    /// settled.
    unsettled: Option<&'static str>,
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
        self.run_all(&loaded, &hooked, &ask, &|_| {}, &|_, _| {})
            .into_iter()
            .filter_map(|(rule, then)| {
                then.map(|then| {
                    json!({ "rule": rule.id(), "action": then.name(), "payload": then.payload() })
                })
            })
            .collect()
    }

    /// Runs `hooked` on the router's pool, calling `announced` as each run
    /// starts and `ran` as each result arrives, and returns what each
    /// returned, in the order given. Each run is written to the log, in that
    /// same order; a rule no worker started before the deadline earns a line
    /// saying it was skipped.
    fn run_all(
        &self,
        loaded: &Arc<Loaded>,
        hooked: &[Arc<Rule>],
        ask: &Ask,
        announced: &(dyn Fn(&Rule) + Sync),
        ran: &(dyn Fn(&Rule, &RuleRun) + Sync),
    ) -> Vec<(Arc<Rule>, Option<Then>)> {
        let mut returned: BTreeMap<usize, RuleRun> = BTreeMap::new();
        let jobs =
            |index: usize, rule: &Arc<Rule>, send: Sender<(usize, Report<Option<RuleRun>>)>| {
                let rule = Arc::clone(rule);
                let loaded = Arc::clone(loaded);
                let cooldowns = Arc::clone(&self.cooldowns);
                let ask = ask.clone();
                let job: Job = Box::new(move || {
                    let announce = send.clone();
                    // The caller's notify belongs to its own thread, so the
                    // job reports the start here and the dispatcher sends it
                    // on.
                    let started = move || {
                        // A caller that stopped waiting drops this, and the
                        // run goes on.
                        let _ = announce.send((index, Report::Started));
                    };
                    // A rule that panics yields nothing, and its worker runs on.
                    let done = panic::catch_unwind(AssertUnwindSafe(|| {
                        Self::run_one(&loaded, &cooldowns, &rule, &ask, &started)
                    }))
                    .ok();
                    let _ = send.send((index, Report::Finished(done)));
                });
                job
            };
        let on_start = |index: usize| {
            if let Some(rule) = hooked.get(index) {
                announced(rule);
            }
        };
        let arrived = |index: usize, done: Option<RuleRun>| {
            let (Some(done), Some(rule)) = (done, hooked.get(index)) else {
                return;
            };
            ran(rule, &done);
            returned.insert(index, done);
        };
        let unstarted = dispatch(&self.rules, ask.deadline, hooked, jobs, on_start, arrived);
        for index in unstarted {
            if let Some(rule) = hooked.get(index) {
                returned.insert(index, Self::skipped(rule, ask));
            }
        }
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
                    unsettled: _,
                } = done;
                // A run still returns when the log cannot be written.
                if let Err(error) = self.audit.append(entry) {
                    eprintln!("chauffeur: a run was not written to the audit log: {error}");
                }
                Some((Arc::clone(rule), then))
            })
            .collect()
    }

    /// A rule's line when its run never started: the deadline came first, so
    /// the log says so. Nothing is returned, and the wire has no event for a
    /// rule that did not run.
    fn skipped(rule: &Rule, ask: &Ask) -> RuleRun {
        RuleRun {
            entry: json!({
                "rule": rule.id(),
                "event": ask.event,
                "session": ask.session,
                "skipped": "deadline",
            }),
            then: None,
            run: None,
            unsettled: None,
        }
    }

    /// One rule's run: its log entry, what it returned, and its run. A rule
    /// in cooldown returns before `announce`, so it is not announced.
    fn run_one(
        loaded: &Arc<Loaded>,
        cooldowns: &Cooldowns,
        rule: &Arc<Rule>,
        ask: &Ask,
        announce: &dyn Fn(),
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
                unsettled: None,
            };
        }
        announce();
        let began = Instant::now();
        let ran = run_strategy(
            &loaded.context,
            rule,
            ask.evidence.as_slice(),
            ask.deadline,
            &ask.cancel,
        );
        let elapsed = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        entry.insert("elapsed_ms".into(), json!(elapsed));
        let returned = ran.then(rule).cloned();
        let unsettled = log_entry::unsettled(&ran);
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
            unsettled,
        }
    }
}

/// What a job reports before and after its run.
enum Report<T> {
    /// Its run starts now, so the rule is announced.
    Started,
    /// Its run ended: `None` if it panicked.
    Finished(T),
}

/// Runs one job per item on `pool`, calling `started` as each job announces
/// that its run begins and `arrived` as each result comes, keeping no more
/// jobs running than the pool has workers: a job that finds none free looks
/// for a finished one before trying again. Past `deadline` the items left do
/// not run, and the results still out are not waited for. Returns the indexes
/// of the items no worker started.
fn dispatch<T, R>(
    pool: &Pool,
    deadline: Instant,
    items: &[R],
    job: impl Fn(usize, &R, Sender<(usize, Report<T>)>) -> Job,
    mut started: impl FnMut(usize),
    mut arrived: impl FnMut(usize, T),
) -> Vec<usize>
where
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
        if let Some((index, report)) = finished(&receive, left.min(LOOK)) {
            match report {
                Report::Started => started(index),
                Report::Finished(done) => {
                    outstanding = outstanding.saturating_sub(1);
                    arrived(index, done);
                }
            }
        }
    }
    // Every job sends exactly one `Finished`, a rule in cooldown and a run
    // that panicked before announcing included, so `outstanding` reaches zero
    // once they have all come. The caller's sender goes first, so the channel
    // closes when the last job's clone does: a job that ends without
    // reporting ends the wait at once instead of holding the caller. The runs
    // stop at `deadline` and report just after it, so the finishes still out
    // are given one grace before the caller stops waiting, and no message at
    // all inside that bound ends the wait too.
    drop(send);
    let bound = deadline.checked_add(GRACE).unwrap_or(deadline);
    while outstanding > 0 {
        let left = bound.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let (index, report) = match receive.recv_timeout(left) {
            Ok(report) => report,
            // Nothing holds a sender, so no run can report. A run still out
            // has its finish on the way, and the bound came first. Either
            // way, waiting longer would be for a message that cannot come.
            Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
        };
        match report {
            Report::Started => started(index),
            Report::Finished(done) => {
                outstanding = outstanding.saturating_sub(1);
                arrived(index, done);
            }
        }
    }
    pending.map(|(index, _)| index).collect()
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

    use chauffeur_protocol::Rule;
    use chauffeur_runtime::{Cancel, Pool, Source};
    use serde_json::json;

    use super::{Ask, Report, Router, dispatch};

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

        let unstarted = dispatch(
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
                    let _ = send.send((index, Report::Finished(index)));
                })
            },
            |_| {},
            |_, value| done.lock().unwrap().push(value),
        );

        assert!(unstarted.is_empty(), "every job started: {unstarted:?}");
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

    /// A job still running when the deadline passes holds its worker, so the
    /// item after it never starts: the dispatcher reports that index and
    /// returns without waiting for the job still out. Each job sleeps for
    /// 500 ms, so a call that waited for one fails the bound instead of
    /// hanging the test.
    #[test]
    fn a_job_past_the_deadline_leaves_the_items_after_it_unstarted() {
        let pool = Pool::new(1, 1);
        let items = [0_usize, 1];
        let announced = Arc::new(Mutex::new(Vec::new()));
        let arrived = Arc::new(Mutex::new(Vec::new()));
        let began = Instant::now();
        let deadline = began
            .checked_add(Duration::from_millis(50))
            .unwrap_or_else(Instant::now);

        let unstarted = dispatch(
            &pool,
            deadline,
            &items,
            |index, item: &usize, send| {
                let item = *item;
                Box::new(move || {
                    let _ = send.send((index, Report::Started));
                    // Longer than the deadline and its grace together.
                    thread::sleep(Duration::from_millis(500));
                    let _ = send.send((index, Report::Finished(item)));
                })
            },
            |index| announced.lock().unwrap().push(index),
            |_, value| arrived.lock().unwrap().push(value),
        );
        let elapsed = began.elapsed();

        assert_eq!(
            unstarted,
            [1],
            "only the item no worker could start comes back"
        );
        assert_eq!(
            *announced.lock().unwrap(),
            [0],
            "the item that started is announced exactly once"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "the call returns well inside the slow jobs: {elapsed:?}"
        );
        assert!(
            arrived.lock().unwrap().is_empty(),
            "the slow job's result comes too late"
        );
    }

    /// A job that reports only its finish, as a rule in cooldown does, does
    /// not hold the caller: the loop ends when the runs do, not when a start
    /// and a finish have each come. The deadline is two seconds off, so a
    /// call that waited for a start that never comes fails the 300 ms bound
    /// instead of hanging the test.
    #[test]
    fn a_job_that_reports_only_its_finish_returns_under_the_deadline() {
        let pool = Pool::new(1, 1);
        let items = [7_usize];
        let arrived = Arc::new(Mutex::new(Vec::new()));
        let began = Instant::now();
        let deadline = began
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);

        let unstarted = dispatch(
            &pool,
            deadline,
            &items,
            |index, item: &usize, send| {
                let item = *item;
                Box::new(move || {
                    // No `Started`: a rule in cooldown returns before it
                    // announces, and reports its finish alone.
                    let _ = send.send((index, Report::Finished(item)));
                })
            },
            |_| {},
            |_, value| arrived.lock().unwrap().push(value),
        );
        let elapsed = began.elapsed();

        assert!(unstarted.is_empty(), "the job started: {unstarted:?}");
        assert_eq!(*arrived.lock().unwrap(), [7], "its finish came back");
        assert!(
            elapsed < Duration::from_millis(300),
            "the call returns without a start: {elapsed:?}"
        );
    }

    /// Every job's run is over as it starts: the jobs do no work and report
    /// one finish each, so the loop has only finishes to count and ends when
    /// the last has come. The deadline is two seconds off, so a call that
    /// also waited for a start per job fails the 300 ms bound instead of
    /// hanging the test.
    #[test]
    fn a_dispatch_whose_runs_are_all_over_before_collecting_returns_at_once() {
        let pool = Pool::new(4, 4);
        let items = [0_usize, 1, 2, 3];
        let arrived = Arc::new(Mutex::new(Vec::new()));
        let began = Instant::now();
        let deadline = began
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);

        let unstarted = dispatch(
            &pool,
            deadline,
            &items,
            |index, item: &usize, send| {
                let item = *item;
                Box::new(move || {
                    let _ = send.send((index, Report::Finished(item)));
                })
            },
            |_| {},
            |_, value| arrived.lock().unwrap().push(value),
        );
        let elapsed = began.elapsed();

        assert!(unstarted.is_empty(), "every job started: {unstarted:?}");
        let mut returned = arrived.lock().unwrap().clone();
        assert_eq!(returned.len(), items.len(), "every finish came back once");
        returned.sort_unstable();
        assert_eq!(returned, items, "each job's finish came back");
        assert!(
            elapsed < Duration::from_millis(300),
            "the call returns without a start per job: {elapsed:?}"
        );
    }

    /// A rule the dispatcher never started is logged as skipped for the
    /// deadline, and returns nothing.
    #[test]
    fn a_rule_that_never_started_is_logged_as_skipped() {
        let rule = Rule::parse(
            "sourcefed/triage",
            r#"{ "on": "sourcefed.event", "strategy": [{ "id": "screen", "judge": "system-one",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Is this noise?" } }] }"#,
        )
        .unwrap();
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(Vec::new()),
            deadline: Instant::now(),
            cancel: Cancel::new(Source::Harness),
        };

        let skipped = Router::skipped(&rule, &ask);

        assert_eq!(
            skipped.entry,
            json!({
                "rule": "sourcefed/triage",
                "event": "sourcefed.event",
                "session": "s-1",
                "skipped": "deadline",
            })
        );
        assert!(skipped.then.is_none() && skipped.run.is_none());
    }
}
