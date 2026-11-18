mod cooldown;
mod log_entry;
mod navigate;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chauffeur_judges::Answer;
use chauffeur_protocol::{Event, Rule, Then};
use chauffeur_runtime::{
    Admission, Busy, Cancel, Capacity, ExecuteError, Job, Permit, Pool, Ran, Source, run_strategy,
};
use cooldown::{Cooldowns, Flights};
use serde_json::{Map, Value, json};

use crate::audit::Audit;
use crate::project;
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
    projects: project::Cache,
    cooldowns: Arc<Cooldowns>,
    flights: Arc<Flights>,
    admission: Admission,
    rules: Pool,
    audit: Arc<Audit>,
}

/// A rule to run, and the project it came from when it is a project rule:
/// owned, so the worker that runs it takes its own copy.
#[derive(Clone)]
struct Hook {
    rule: Arc<Rule>,
    project: Option<PathBuf>,
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
            projects: project::Cache::new(),
            cooldowns: Arc::new(Cooldowns::new()),
            flights: Arc::new(Flights::new()),
            admission: Admission::new(Capacity::DAEMON),
            rules: Pool::new(RULES_KEPT, RULES_ALIVE),
            audit: Arc::new(audit),
        }
    }

    /// Replaces the loaded rules, models and redaction; the next event uses
    /// them. A project's cached rules came from the older load, so the next
    /// navigation reloads them.
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
        let hooked: Vec<Hook> = loaded
            .rules
            .iter()
            .filter(|rule| rule.hooks(event))
            .cloned()
            .map(|rule| Hook {
                rule,
                project: None,
            })
            .collect();
        let ask = Ask {
            event: event.name().to_owned(),
            session: event.session().unwrap_or_default().to_owned(),
            evidence: Arc::new(event.evidence()),
            deadline,
            cancel: cancel.clone(),
        };
        self.run_all(&loaded, &hooked, &ask, &Instant::now, &|_| {}, &|_, _| {})
            .into_iter()
            .filter_map(|(rule, then)| {
                then.map(|then| {
                    json!({ "rule": rule.id(), "action": then.name(), "payload": then.payload() })
                })
            })
            .collect()
    }

    /// Runs `hooked` on the router's pool, calling `announced` as each run
    /// starts and `ran` for each result whose run was announced, and returns
    /// what each returned, in the order given. Each run is written to the log,
    /// in that same order; a rule no worker started before the deadline earns
    /// a line saying it was skipped, and one whose run did not settle earns the
    /// line its end writes. A result fires its rule's cooldown only when it is
    /// delivered here, so one the caller never hears leaves the rule able to
    /// run again. `now` is the clock the dispatcher reads the deadline and its
    /// grace from, so a test can move it past both without waiting.
    fn run_all(
        &self,
        loaded: &Arc<Loaded>,
        hooked: &[Hook],
        ask: &Ask,
        now: &dyn Fn() -> Instant,
        announced: &(dyn Fn(&Hook) + Sync),
        ran: &(dyn Fn(&Rule, &RuleRun) + Sync),
    ) -> Vec<(Arc<Rule>, Option<Then>)> {
        let mut returned: BTreeMap<usize, RuleRun> = BTreeMap::new();
        let jobs = |index: usize, hook: &Hook, send: Sender<(usize, Report<Option<RuleRun>>)>| {
            let Hook { rule, project } = hook.clone();
            let loaded = Arc::clone(loaded);
            let cooldowns = Arc::clone(&self.cooldowns);
            let flights = Arc::clone(&self.flights);
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
                    Self::run_one(
                        &loaded,
                        &cooldowns,
                        &flights,
                        &rule,
                        project.as_deref(),
                        &ask,
                        &started,
                    )
                }))
                .ok();
                let _ = send.send((index, Report::Finished(done)));
            });
            job
        };
        let on_start = |index: usize| {
            if let Some(hook) = hooked.get(index) {
                announced(hook);
            }
        };
        let arrived = |index: usize, done: Option<RuleRun>, began: bool| {
            let Some(hook) = hooked.get(index) else {
                return;
            };
            // A job that panicked reports no run; its rule is still settled,
            // as a worker that stopped.
            let mut done = done.unwrap_or_else(|| {
                Self::unsettled(&hook.rule, hook.project.as_deref(), ask, Ran::Stopped)
            });
            // Only a result that is handed back may fire the rule's cooldown.
            delivered(&self.cooldowns, hook, ask, &mut done);
            // A rule the dispatcher never heard start was never announced,
            // so its steps settle on no wire; its line is still written.
            if began {
                ran(&hook.rule, &done);
            }
            returned.insert(index, done);
        };
        let left = dispatch(
            &self.rules,
            ask.deadline,
            now,
            hooked,
            jobs,
            on_start,
            arrived,
        );
        Self::settle(left, hooked, ask, &self.cooldowns, ran, &mut returned);
        self.logged(hooked, returned)
    }

    /// Writes each run to the log, in hook order, and gives back each rule and
    /// what it returned.
    fn logged(
        &self,
        hooked: &[Hook],
        returned: BTreeMap<usize, RuleRun>,
    ) -> Vec<(Arc<Rule>, Option<Then>)> {
        // The indexes are the hooks', so a BTreeMap gives the runs in hook
        // order, and the audit reads in the order the rules load.
        returned
            .into_iter()
            .filter_map(|(index, done)| {
                let hook = hooked.get(index)?;
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
                Some((Arc::clone(&hook.rule), then))
            })
            .collect()
    }

    /// Settles what a dispatch left undone, in hook order: a run the
    /// dispatcher heard start and never finish is settled as one cut at the
    /// deadline, one no worker could be started for says so with its reason,
    /// and one no worker started is skipped. A rule the dispatcher never
    /// heard start was never announced, so its steps settle on no wire though
    /// its line is still written; one of those whose rule is cooling is
    /// logged as the cooldown it shows. A project rule's line names its
    /// project, as its run's would.
    fn settle(
        left: Left,
        hooked: &[Hook],
        ask: &Ask,
        cooldowns: &Cooldowns,
        ran: &(dyn Fn(&Rule, &RuleRun) + Sync),
        returned: &mut BTreeMap<usize, RuleRun>,
    ) {
        // A run a worker started and never finished is settled as a run cut
        // at the deadline, so no event leaves a rule half logged.
        for index in left.open {
            let Some(hook) = hooked.get(index) else {
                continue;
            };
            let began = left.begun.contains(&index);
            let done = if began {
                Self::unsettled(&hook.rule, hook.project.as_deref(), ask, Ran::Deadline)
            } else if cooldowns.cooling(
                &cooldown_key(hook.project.as_deref(), &hook.rule, &ask.session),
                hook.rule.cooldown(),
            ) {
                // A run that never reported: the cooldown it shows now is the
                // best available answer for a run that never announced.
                Self::cooled(&hook.rule, hook.project.as_deref(), ask)
            } else {
                Self::unsettled(&hook.rule, hook.project.as_deref(), ask, Ran::Deadline)
            };
            if began {
                ran(&hook.rule, &done);
            }
            returned.insert(index, done);
        }
        // A run no worker could be started for says so, with the reason. It
        // was never announced, so it settles no step on the wire.
        for (index, reason) in left.refused {
            let Some(hook) = hooked.get(index) else {
                continue;
            };
            let done = Self::unsettled(
                &hook.rule,
                hook.project.as_deref(),
                ask,
                Ran::Refused(reason),
            );
            if left.begun.contains(&index) {
                ran(&hook.rule, &done);
            }
            returned.insert(index, done);
        }
        for index in left.unstarted {
            if let Some(hook) = hooked.get(index) {
                returned.insert(
                    index,
                    Self::skipped(&hook.rule, hook.project.as_deref(), ask),
                );
            }
        }
    }

    /// A rule's line when its run never started: the deadline came first, so
    /// the log says so. Nothing is returned, and the wire has no event for a
    /// rule that did not run. A project rule's line names its project, as its
    /// run's would.
    fn skipped(rule: &Rule, project: Option<&Path>, ask: &Ask) -> RuleRun {
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(rule.id()));
        if let Some(project) = project {
            entry.insert("project".into(), json!(project.display().to_string()));
        }
        entry.insert("event".into(), json!(ask.event));
        entry.insert("session".into(), json!(ask.session));
        entry.insert("skipped".into(), json!("deadline"));
        RuleRun {
            entry: Value::Object(entry),
            then: None,
            run: None,
            unsettled: None,
        }
    }

    /// A rule's line when its run was in cooldown: no judge was asked, and
    /// nothing is returned, so the wire has no event for it. A project rule's
    /// line names its project, as its run's would.
    fn cooled(rule: &Rule, project: Option<&Path>, ask: &Ask) -> RuleRun {
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(rule.id()));
        if let Some(project) = project {
            entry.insert("project".into(), json!(project.display().to_string()));
        }
        entry.insert("event".into(), json!(ask.event));
        entry.insert("session".into(), json!(ask.session));
        entry.insert("cooldown".into(), json!(true));
        RuleRun {
            entry: Value::Object(entry),
            then: None,
            run: None,
            unsettled: None,
        }
    }

    /// A rule's line when its run started and did not settle: the runtime's
    /// own line for that end, and the word its steps settle with. Nothing is
    /// returned. A project rule's line names its project, as its run's would.
    fn unsettled(rule: &Rule, project: Option<&Path>, ask: &Ask, ran: Ran) -> RuleRun {
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(rule.id()));
        if let Some(project) = project {
            entry.insert("project".into(), json!(project.display().to_string()));
        }
        entry.insert("event".into(), json!(ask.event));
        entry.insert("session".into(), json!(ask.session));
        let word = log_entry::unsettled(&ran);
        // For these ends the runtime gives no run back.
        let run = log_entry::ended(&mut entry, ran);
        RuleRun {
            entry: Value::Object(entry),
            then: None,
            run,
            unsettled: word,
        }
    }

    /// One rule's run: its log entry, what it returned, and its run. A rule
    /// in cooldown returns before `announce`, so it is not announced; one with
    /// a run still out does the same, and both are logged as cooling. A
    /// project's rule keys both by that project, so two projects with a rule
    /// of the same ID do not suppress each other, and its entry names the
    /// project it came from. A result it returns fires no cooldown here: only
    /// a result the caller delivers does. Its place in the in-flight set is
    /// given back when it returns, however it ends, so a result the caller
    /// never delivers still lets the rule run again.
    ///
    /// A cancel and the deadline stop the caller waiting for a run, not the
    /// judge: the runtime drops a late result, while a judge already asked
    /// runs on to its own model timeout. Both are read before each round,
    /// never inside one.
    fn run_one(
        loaded: &Arc<Loaded>,
        cooldowns: &Cooldowns,
        flights: &Flights,
        rule: &Arc<Rule>,
        project: Option<&Path>,
        ask: &Ask,
        announce: &dyn Fn(),
    ) -> RuleRun {
        let key = cooldown_key(project, rule, &ask.session);
        if cooldowns.cooling(&key, rule.cooldown()) {
            return Self::cooled(rule, project, ask);
        }
        // A run already out holds one worker, not one per event: a judge that
        // never returns leaves the rest of the pool free for other rules.
        let Some(_flight) = flights.begin(key) else {
            return Self::cooled(rule, project, ask);
        };
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(rule.id()));
        if let Some(project) = project {
            entry.insert("project".into(), json!(project.display().to_string()));
        }
        entry.insert("event".into(), json!(ask.event));
        entry.insert("session".into(), json!(ask.session));
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
        let then = ran.then(rule).cloned();
        let unsettled = log_entry::unsettled(&ran);
        let run = log_entry::ended(&mut entry, ran);
        RuleRun {
            entry: Value::Object(entry),
            then,
            run,
            unsettled,
        }
    }
}

/// The triple a rule's cooldown is keyed by: the project it came from when it
/// is a project's rule, its ID, and the session it ran in.
fn cooldown_key(project: Option<&Path>, rule: &Rule, session: &str) -> cooldown::Key {
    (
        project.map(Path::to_path_buf),
        rule.id().to_owned(),
        session.to_owned(),
    )
}

/// Fires the rule's cooldown for a result about to be delivered, and writes
/// its action into the run's entry. A run whose cooldown another run already
/// fired in this window gives its action up and says so.
///
/// Firing here, rather than on the worker, keeps a result that comes after
/// the caller stopped waiting from leaving the rule cooling although its
/// action was never handed back.
fn delivered(cooldowns: &Cooldowns, hook: &Hook, ask: &Ask, done: &mut RuleRun) {
    let Some(name) = done.then.as_ref().map(|then| then.name().to_owned()) else {
        return;
    };
    let key = cooldown_key(hook.project.as_deref(), &hook.rule, &ask.session);
    if !cooldowns.try_fire(key, hook.rule.cooldown()) {
        if let Value::Object(entry) = &mut done.entry {
            entry.insert("cooldown".into(), json!(true));
        }
        done.then = None;
        return;
    }
    if let Value::Object(entry) = &mut done.entry {
        entry.insert("returned".into(), json!(name));
    }
}

/// What a job reports before and after its run.
enum Report<T> {
    /// Its run starts now, so the rule is announced.
    Started,
    /// Its run ended: `None` if it panicked.
    Finished(T),
}

/// What a dispatch left undone: the runs a worker started and did not finish,
/// the items no worker started, the items a worker could not be started for,
/// with why, and the items whose run was heard to start.
struct Left {
    /// Handed to a worker, with no finish before the wait ended.
    open: BTreeSet<usize>,
    /// No worker started: the deadline came first.
    unstarted: Vec<usize>,
    /// A worker could not be started: why, such as the OS error.
    refused: Vec<(usize, String)>,
    /// Announced their run's start before the wait ended, whether or not
    /// their finish then came. A rule not here was never announced, so its
    /// steps settle on no wire.
    begun: BTreeSet<usize>,
}

/// The items a dispatch has out with its workers: those handed to one and
/// not finished, those whose run was heard to start, and how many are still
/// out.
#[derive(Default)]
struct Out {
    /// Handed to a worker, with no finish yet.
    open: BTreeSet<usize>,
    /// Announced their run's start, whether or not their finish then came.
    begun: BTreeSet<usize>,
    /// How many have not reported a finish.
    outstanding: usize,
}

impl Out {
    /// Takes one report from a worker: a start is recorded and announced, and
    /// a finish settles the item, with whether its run was heard to begin.
    fn take<T>(
        &mut self,
        index: usize,
        report: Report<T>,
        started: &mut impl FnMut(usize),
        arrived: &mut impl FnMut(usize, T, bool),
    ) {
        match report {
            Report::Started => {
                self.begun.insert(index);
                started(index);
            }
            Report::Finished(done) => {
                let heard = self.begun.contains(&index);
                self.outstanding = self.outstanding.saturating_sub(1);
                self.open.remove(&index);
                arrived(index, done, heard);
            }
        }
    }
}

/// Runs one job per item on `pool`, calling `started` as each job announces
/// that its run begins and `arrived` as each result comes with whether that
/// run was heard to begin, keeping no more jobs running than the pool has
/// workers: a job that finds none free looks for a finished one before trying
/// again. A job no worker can be started for is refused with its reason
/// rather than tried again, and one that panics after it starts still reports
/// its finish. Past `deadline` the items left do not run, and the runs still
/// out are not waited for. `now` is the clock both bounds are read from, so a
/// caller can move it past them without waiting. Returns what it left undone.
fn dispatch<T, R>(
    pool: &Pool,
    deadline: Instant,
    now: &dyn Fn() -> Instant,
    items: &[R],
    job: impl Fn(usize, &R, Sender<(usize, Report<T>)>) -> Job,
    mut started: impl FnMut(usize),
    mut arrived: impl FnMut(usize, T, bool),
) -> Left
where
    T: Send + 'static,
{
    let (send, receive) = mpsc::channel();
    let mut pending = items.iter().enumerate().peekable();
    let mut out = Out::default();
    let mut refused = Vec::new();
    while let Some(&(index, item)) = pending.peek() {
        match pool.execute(job(index, item, send.clone())) {
            Ok(()) => {
                pending.next();
                // Handed to a worker: its finish, or the wait's end, settles it.
                out.open.insert(index);
                out.outstanding = out.outstanding.saturating_add(1);
                continue;
            }
            Err(ExecuteError::Busy) => {}
            // Waiting cannot free a thread, so the item is refused with the
            // reason instead of tried again.
            Err(error) => {
                pending.next();
                refused.push((index, error.to_string()));
                continue;
            }
        }
        // No worker is free: a run has to finish before this one can start.
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() {
            break;
        }
        if let Some((index, report)) = finished(&receive, left.min(LOOK)) {
            out.take(index, report, &mut started, &mut arrived);
        }
    }
    // Every job sends exactly one `Finished`, a rule in cooldown and a run
    // that panicked before announcing included, so the count reaches zero
    // once they have all come. The caller's sender goes first, so the channel
    // closes when the last job's clone does: a job that ends without
    // reporting ends the wait at once instead of holding the caller. The runs
    // stop at `deadline` and report just after it, so the finishes still out
    // are given one grace before the caller stops waiting, and no message at
    // all inside that bound ends the wait too.
    drop(send);
    let bound = deadline.checked_add(GRACE).unwrap_or(deadline);
    loop {
        if out.outstanding == 0 {
            break;
        }
        let left = bound.saturating_duration_since(now());
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
        out.take(index, report, &mut started, &mut arrived);
    }
    Left {
        open: out.open,
        unstarted: pending.map(|(index, _)| index).collect(),
        refused,
        begun: out.begun,
    }
}

/// One finished result, if one comes within `wait`.
fn finished<T>(receive: &Receiver<(usize, T)>, wait: Duration) -> Option<(usize, T)> {
    receive.recv_timeout(wait).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use chauffeur_protocol::Rule;
    use chauffeur_runtime::{Cancel, Pool, Ran, Source};
    use serde_json::{Value, json};

    use super::cooldown::{Cooldowns, Flights};
    use super::{Ask, GRACE, Hook, Left, Report, Router, RuleRun, delivered, dispatch, finished};
    use crate::Paths;
    use crate::audit::{Audit, MAX_BYTES};
    use crate::state::Loaded;

    /// A rule hooking `sourcefed.event`, for a test that settles its run
    /// without running it.
    fn hook(id: &str, project: Option<&str>) -> Hook {
        let rule = Rule::parse(
            id,
            r#"{ "on": "sourcefed.event", "strategy": [{ "id": "screen", "judge": "system-one",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Is this noise?" } }] }"#,
        )
        .unwrap();
        Hook {
            rule: Arc::new(rule),
            project: project.map(PathBuf::from),
        }
    }

    /// A rule with a cooldown that settles at once against a stand-in and
    /// returns its intent.
    fn settling_rule() -> Arc<Rule> {
        Arc::new(
            Rule::parse(
                "sourcefed/triage",
                r#"{ "on": "sourcefed.event", "cooldown_seconds": 60,
                     "strategy": [{ "id": "screen", "judge": "system-one",
                         "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                                  "instructions": "Is this noise?" } }],
                     "then": [{ "if": { "screen": "yes" }, "label": "noise" }] }"#,
            )
            .unwrap(),
        )
    }

    /// A uniquely named temporary folder, emptied, as the filesystem
    /// resolves it.
    fn temporary(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "chauffeur-route-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the temporary folder should be writable");
        fs::canonicalize(&path).expect("the temporary folder should resolve")
    }

    /// A load whose `models.json` reads `models`, and the temporary folder it
    /// lives in.
    fn loading_models(name: &str, models: &str) -> (PathBuf, Loaded) {
        let root = temporary(name);
        let distribution = root.join("distribution");
        fs::create_dir_all(distribution.join("rules")).expect("the shipped rules folder");
        fs::create_dir_all(distribution.join("safety")).expect("the shipped safety folder");
        fs::write(
            distribution.join("safety/redaction.json"),
            r#"{ "prefixes": [] }"#,
        )
        .expect("the redaction file");
        let file = root.join("models.json");
        fs::write(&file, models).expect("the models file");
        let paths = Paths {
            name: "chauffeur-test".into(),
            socket: root.join("run/daemon.sock"),
            config: root.join("config"),
            distribution,
            state: root.join("state"),
            models: Some(file),
        };
        let loaded = crate::state::load(&paths, None).expect("the fixture loads");

        (root, loaded)
    }

    /// A load whose system-one tier is a stand-in that answers at once, and
    /// the temporary folder it lives in.
    fn loading(name: &str) -> (PathBuf, Loaded) {
        loading_models(
            name,
            r#"{ "models": { "stand-in": { "format": "placeholder", "otherwise": { "yes": 0.94, "no": 0.06 } } }, "tiers": { "system-one": ["stand-in"] } }"#,
        )
    }

    /// A load whose System One model waits for its key on a channel the test
    /// holds, so a run hangs until the test drops `release`; the key is then
    /// missing, and the run ends with no judge. `waiting` says a run has
    /// reached the wait, so a test knows the key is held.
    struct Hanging {
        root: PathBuf,
        loaded: Loaded,
        release: mpsc::Sender<()>,
        waiting: mpsc::Receiver<()>,
    }

    fn hanging(name: &str) -> Hanging {
        let (root, mut loaded) = loading_models(
            name,
            r#"{ "models": { "hung": { "format": "systemone", "endpoint": "http://127.0.0.1:9", "model": "stand-in", "key": { "env": "CHAUFFEUR_TEST_KEY" } } }, "tiers": { "system-one": ["hung"] } }"#,
        );
        let (release, held) = mpsc::channel();
        let (waiting, waited) = mpsc::channel();
        let held = Mutex::new(held);
        let context = Arc::get_mut(&mut loaded.context).expect("the fixture's context is its own");
        context.keys = Box::new(move |_name: &str| {
            let _ = waiting.send(());
            if let Ok(held) = held.lock() {
                let _ = held.recv();
            }
            None
        });

        Hanging {
            root,
            loaded,
            release,
            waiting: waited,
        }
    }

    /// The audit's lines, parsed, with each line's time taken out so two
    /// runs' lines compare.
    fn written(folder: &Path) -> Vec<Value> {
        crate::audit::recent(folder, 8)
            .expect("the audit log reads")
            .iter()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).expect("an audit line is JSON");
                if let Value::Object(fields) = &mut entry {
                    fields.remove("at");
                }
                entry
            })
            .collect()
    }

    /// A clock a test moves by hand, so a deadline and its grace pass without
    /// waiting.
    struct Clock {
        now: Mutex<Instant>,
    }

    impl Clock {
        fn at(now: Instant) -> Self {
            Self {
                now: Mutex::new(now),
            }
        }

        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn set(&self, now: Instant) {
            *self.now.lock().unwrap() = now;
        }

        /// The clock moved past `deadline` and `GRACE` together.
        fn past(&self, deadline: Instant) {
            let bound = deadline.checked_add(GRACE).unwrap_or(deadline);
            let past = bound.checked_add(Duration::from_millis(1)).unwrap_or(bound);
            self.set(past);
        }
    }

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

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
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
            |_, value, _| done.lock().unwrap().push(value),
        );

        assert!(
            left.unstarted.is_empty(),
            "every job started: {:?}",
            left.unstarted
        );
        assert!(left.open.is_empty(), "every job finished: {:?}", left.open);
        assert!(
            left.refused.is_empty(),
            "no job was refused: {:?}",
            left.refused
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

    /// A job still running when the deadline passes holds its worker, so the
    /// item after it never starts: the dispatcher leaves the started index
    /// open, reports the later one as unstarted, and returns without waiting
    /// for the job still out. Each job sleeps for 500 ms, so a call that
    /// waited for one fails the bound instead of hanging the test.
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

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
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
            |_, value, _| arrived.lock().unwrap().push(value),
        );
        let elapsed = began.elapsed();

        assert_eq!(
            left.open.into_iter().collect::<Vec<_>>(),
            [0],
            "the item whose run outlived the deadline is left open"
        );
        assert_eq!(
            left.unstarted,
            [1],
            "only the item no worker could start comes back unstarted"
        );
        assert!(
            left.refused.is_empty(),
            "no item was refused: {:?}",
            left.refused
        );
        assert_eq!(
            left.begun,
            BTreeSet::from([0]),
            "the item whose run announced its start is begun"
        );
        assert_eq!(
            *announced.lock().unwrap(),
            [0],
            "the item that started is announced exactly once"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "the call returns well inside the slow jobs: {elapsed:?}"
        );
        assert!(
            arrived.lock().unwrap().is_empty(),
            "the slow job's result comes too late"
        );
    }

    /// A job that announces its start and then ends with no result, as a run
    /// that panicked does, is reported with that finish: the dispatcher
    /// neither leaves it open nor waits for a second report.
    #[test]
    fn a_job_that_ends_with_no_result_after_it_starts_is_reported() {
        let pool = Pool::new(1, 1);
        let items = [0_usize];
        let arrived = Arc::new(Mutex::new(Vec::new()));
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
            &items,
            |index, _item: &usize, send| {
                Box::new(move || {
                    let _ = send.send((index, Report::Started));
                    // A run that panicked reports no result, and its worker
                    // runs on.
                    let _ = send.send((index, Report::Finished(None::<usize>)));
                })
            },
            |_| {},
            |index, done, began| {
                assert!(began, "the finish follows the start it reports");
                arrived.lock().unwrap().push((index, done));
            },
        );

        assert_eq!(
            *arrived.lock().unwrap(),
            [(0, None)],
            "the finish is reported, with no result"
        );
        assert!(
            left.open.is_empty(),
            "the run out is settled: {:?}",
            left.open
        );
        assert!(
            left.unstarted.is_empty(),
            "the item started: {:?}",
            left.unstarted
        );
        assert!(
            left.refused.is_empty(),
            "no item was refused: {:?}",
            left.refused
        );
    }

    /// A job that reports only its finish, as a rule in cooldown does, does
    /// not hold the caller: the loop ends when the runs do, not when a start
    /// and a finish have each come. The deadline is two seconds off, so a
    /// call that waited for a start that never comes fails the 1 s bound
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

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
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
            |index, value, heard| {
                assert!(
                    !heard,
                    "a finish with no start was not heard to begin: {index}"
                );
                arrived.lock().unwrap().push(value);
            },
        );
        let elapsed = began.elapsed();

        assert!(
            left.unstarted.is_empty(),
            "the job started: {:?}",
            left.unstarted
        );
        assert_eq!(*arrived.lock().unwrap(), [7], "its finish came back");
        assert!(
            elapsed < Duration::from_secs(1),
            "the call returns without a start: {elapsed:?}"
        );
    }

    /// Every job's run is over as it starts: the jobs do no work and report
    /// one finish each, so the loop has only finishes to count and ends when
    /// the last has come. The deadline is two seconds off, so a call that
    /// also waited for a start per job fails the 1 s bound instead of
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

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
            &items,
            |index, item: &usize, send| {
                let item = *item;
                Box::new(move || {
                    let _ = send.send((index, Report::Finished(item)));
                })
            },
            |_| {},
            |_, value, _| arrived.lock().unwrap().push(value),
        );
        let elapsed = began.elapsed();

        assert!(
            left.unstarted.is_empty(),
            "every job started: {:?}",
            left.unstarted
        );
        let mut returned = arrived.lock().unwrap().clone();
        assert_eq!(returned.len(), items.len(), "every finish came back once");
        returned.sort_unstable();
        assert_eq!(returned, items, "each job's finish came back");
        assert!(
            elapsed < Duration::from_secs(1),
            "the call returns without a start per job: {elapsed:?}"
        );
    }

    /// A start reaches the caller while the dispatcher is still starting
    /// jobs, not after the last one has started. One worker runs two jobs:
    /// the first holds its worker until its start is announced, so the
    /// second can only find a worker free once the announcement has come,
    /// and the two finishes follow in the order the jobs began.
    #[test]
    fn a_start_is_announced_before_the_job_that_waits_for_a_worker_begins() {
        let pool = Pool::new(1, 1);
        let items = [0_usize, 1];
        let order = Arc::new(Mutex::new(Vec::new()));
        let next = Arc::new(AtomicUsize::new(0));
        let announced = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .unwrap_or_else(Instant::now);

        let left = dispatch(
            &pool,
            deadline,
            &Instant::now,
            &items,
            |index, item: &usize, send| {
                let (item, order, announced) = (*item, Arc::clone(&order), Arc::clone(&announced));
                Box::new(move || {
                    order.lock().unwrap().push(format!("job {item} began"));
                    let _ = send.send((index, Report::Started));
                    // The first job gives up its worker only once its start
                    // has been announced, so the second has to wait for it.
                    if item == 0 {
                        let began = Instant::now();
                        while !announced.load(Ordering::SeqCst)
                            && began.elapsed() < Duration::from_secs(1)
                        {
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                    let _ = send.send((index, Report::Finished(item)));
                })
            },
            |index| {
                order
                    .lock()
                    .unwrap()
                    .push(format!("job {index} was announced"));
                if index == 0 {
                    announced.store(true, Ordering::SeqCst);
                }
            },
            |index, value, _| {
                assert_eq!(
                    next.fetch_add(1, Ordering::SeqCst),
                    index,
                    "each finish comes in the order its job began"
                );
                assert_eq!(value, index, "a finish is the job's own");
                order.lock().unwrap().push(format!("finish {index} came"));
            },
        );

        assert!(left.unstarted.is_empty(), "every job started");
        let order = order.lock().unwrap().clone();
        let at = |step: &str| {
            order
                .iter()
                .position(|entry| entry == step)
                .unwrap_or_else(|| panic!("{step} never came: {order:?}"))
        };
        assert!(
            at("job 0 was announced") < at("job 1 began"),
            "the run that waits is announced before it begins: {order:?}"
        );
        assert_eq!(
            order
                .iter()
                .filter(|step| step.starts_with("finish"))
                .count(),
            items.len(),
            "every finish came once: {order:?}"
        );
    }

    /// A result sent before the wait comes back with its index, and a look
    /// at the now-empty channel comes back with none after the short wait.
    /// Ten ms each: enough to take what is there, too short to hold a test.
    #[test]
    fn a_finished_result_comes_back_within_its_wait_and_none_comes_after() {
        let (send, receive) = mpsc::channel();
        send.send((2_usize, "done")).unwrap();
        let wait = Duration::from_millis(10);

        let first = finished(&receive, wait);

        assert_eq!(
            first,
            Some((2, "done")),
            "the result sent comes back with its index"
        );

        let began = Instant::now();
        let second = finished(&receive, wait);
        let elapsed = began.elapsed();

        assert_eq!(second, None, "the empty channel yields nothing");
        assert!(
            elapsed < Duration::from_secs(1),
            "the empty channel returns after its wait: {elapsed:?}"
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

        let skipped = Router::skipped(&rule, None, &ask);

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

    /// A started run that did not settle writes the runtime's own line for
    /// its end, and its steps settle with the same word: the deadline for a
    /// run still out, and a stopped worker for one that panicked after it
    /// started.
    #[test]
    fn a_started_run_that_did_not_settle_is_logged_as_it_ended() {
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
        let cases: [(Ran, Value, &str); 2] = [
            (
                Ran::Deadline,
                json!({
                    "rule": "sourcefed/triage",
                    "event": "sourcefed.event",
                    "session": "s-1",
                    "deadline": true,
                }),
                "deadline",
            ),
            (
                Ran::Stopped,
                json!({
                    "rule": "sourcefed/triage",
                    "event": "sourcefed.event",
                    "session": "s-1",
                    "refused": "the run's worker stopped",
                }),
                "stopped",
            ),
        ];

        for (ran, logged, word) in cases {
            let done = Router::unsettled(&rule, None, &ask, ran);

            assert_eq!(done.entry, logged);
            assert_eq!(done.unsettled, Some(word), "the word for {logged}");
            assert!(done.then.is_none() && done.run.is_none());
        }
    }

    /// What a dispatch leaves undone is settled in hook order: a run a worker
    /// started is settled as one cut at the deadline, one no worker could
    /// start as refused with its reason, and one no worker started as
    /// skipped. Only the run the dispatcher heard start reaches `ran`: a
    /// refusal was never announced, so its line is written and no step
    /// settles. A project rule's line names its project, and a loaded rule's
    /// does not.
    #[test]
    fn settle_logs_each_run_it_is_left_with_at_its_own_end() {
        let hooked = [
            hook("acme/triage", Some("/projects/acme")),
            hook("sourcefed/triage", None),
            hook("acme/comments", Some("/projects/acme")),
        ];
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(Vec::new()),
            deadline: Instant::now(),
            cancel: Cancel::new(Source::Harness),
        };
        let left = Left {
            open: std::iter::once(0).collect(),
            refused: vec![(1, "no system-one model is registered".to_owned())],
            unstarted: vec![2],
            begun: std::iter::once(0).collect(),
        };
        let cooldowns = Cooldowns::new();
        let deadline = json!({
            "rule": "acme/triage",
            "project": "/projects/acme",
            "event": "sourcefed.event",
            "session": "s-1",
            "deadline": true,
        });
        let refused = json!({
            "rule": "sourcefed/triage",
            "event": "sourcefed.event",
            "session": "s-1",
            "refused": "no system-one model is registered",
        });
        let skipped = json!({
            "rule": "acme/comments",
            "project": "/projects/acme",
            "event": "sourcefed.event",
            "session": "s-1",
            "skipped": "deadline",
        });
        let reported = Mutex::new(Vec::new());
        let ran = |rule: &Rule, done: &RuleRun| {
            let mut calls = reported.lock().unwrap();
            calls.push((rule.id().to_owned(), done.entry.clone()));
        };
        let mut returned = BTreeMap::new();

        Router::settle(left, &hooked, &ask, &cooldowns, &ran, &mut returned);

        let calls = reported.lock().unwrap().clone();
        assert_eq!(
            calls,
            [("acme/triage".to_owned(), deadline.clone())],
            "only the run that was announced settles a step"
        );
        let indexes: Vec<usize> = returned.keys().copied().collect();
        assert_eq!(indexes, [0, 1, 2], "each index is settled exactly once");
        let settled = [
            (0, deadline, Some("deadline")),
            (1, refused, Some("refused")),
            (2, skipped, None),
        ];
        for (index, entry, word) in settled {
            let done = returned.get(&index).unwrap();
            assert_eq!(done.entry, entry, "the entry for index {index}");
            assert_eq!(done.unsettled, word, "the word for index {index}");
            assert!(done.then.is_none() && done.run.is_none());
        }
    }

    /// A run still out for a rule that cooled down before the dispatcher
    /// stopped waiting is settled as the cooldown it shows: for a run that
    /// never reported, that is the best available answer. The cooldown comes
    /// from a result delivered before, and the run that never announced
    /// settles no step.
    #[test]
    fn an_open_run_whose_rule_is_cooling_settles_as_the_cooldown() {
        let (root, loaded) = loading("settle-cooling");
        let loaded = Arc::new(loaded);
        let cooldowns = Cooldowns::new();
        let flights = Flights::new();
        let rule = settling_rule();
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(vec![("event".to_owned(), "CI failed".to_owned())]),
            deadline: Instant::now()
                .checked_add(Duration::from_secs(5))
                .unwrap_or_else(Instant::now),
            cancel: Cancel::new(Source::Harness),
        };
        let hook = Hook {
            rule: Arc::clone(&rule),
            project: None,
        };
        let mut done = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &|| {});
        assert!(done.then.is_some(), "the first run settles: {}", done.entry);
        delivered(&cooldowns, &hook, &ask, &mut done);

        let hooked = [hook];
        let left = Left {
            open: std::iter::once(0).collect(),
            refused: Vec::new(),
            unstarted: Vec::new(),
            begun: BTreeSet::new(),
        };
        let reported = Mutex::new(Vec::new());
        let ran = |rule: &Rule, _done: &RuleRun| {
            reported.lock().unwrap().push(rule.id().to_owned());
        };
        let mut returned = BTreeMap::new();

        Router::settle(left, &hooked, &ask, &cooldowns, &ran, &mut returned);

        assert_eq!(
            returned.get(&0).expect("the run is settled").entry,
            json!({
                "rule": "sourcefed/triage",
                "event": "sourcefed.event",
                "session": "s-1",
                "cooldown": true,
            }),
            "a cooling rule's run is logged as the cooldown it shows"
        );
        assert!(
            reported.lock().unwrap().is_empty(),
            "a run that never announced settles no step"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A run that settles inside its deadline but whose report comes after
    /// the dispatcher stopped waiting is never delivered, so it must not cool
    /// the rule: firing happens when a result is handed back. The next run
    /// then settles and returns its action, and delivering that fires the
    /// cooldown for the one after it.
    #[test]
    fn a_result_reported_after_the_wait_does_not_cool_the_rule() {
        let (root, loaded) = loading("late-report");
        let loaded = Arc::new(loaded);
        let cooldowns = Cooldowns::new();
        let flights = Flights::new();
        let rule = settling_rule();
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(vec![("event".to_owned(), "CI failed".to_owned())]),
            deadline: Instant::now()
                .checked_add(Duration::from_secs(5))
                .unwrap_or_else(Instant::now),
            cancel: Cancel::new(Source::Harness),
        };
        let announce = || {};

        // The worker's half of a run whose caller stopped waiting before its
        // report came: it settles, and nothing hands its action back.
        let late = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &announce);
        assert!(late.then.is_some(), "the late run settled: {}", late.entry);

        // The next run is not cooling: the undelivered result fired nothing.
        let mut next = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &announce);
        assert!(
            next.entry.get("cooldown").is_none(),
            "an undelivered result left the rule cooling: {}",
            next.entry
        );
        assert!(next.then.is_some(), "the next run settled: {}", next.entry);

        // Delivered, a result fires the cooldown, and the run after it is
        // skipped.
        let hook = Hook {
            rule: Arc::clone(&rule),
            project: None,
        };
        delivered(&cooldowns, &hook, &ask, &mut next);
        assert_eq!(
            next.entry.get("returned"),
            Some(&json!("label")),
            "the delivered result says what it returned"
        );
        let after = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &announce);
        assert_eq!(
            after.entry.get("cooldown"),
            Some(&json!(true)),
            "delivery fired the cooldown: {}",
            after.entry
        );
        assert!(after.then.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// An announced run that never finished is settled at the deadline, and
    /// the rule behind it, which no worker could start, is skipped with no
    /// event. The clock moves past the deadline and its grace as the first run
    /// starts, so nothing waits for the judge, which waits on a key the test
    /// never sends. One worker means the second rule's run cannot start while
    /// the first is out.
    #[test]
    fn a_run_still_out_at_the_deadline_settles_it_and_skips_the_rule_behind_it() {
        let Hanging {
            root,
            loaded,
            release,
            waiting: _waiting,
        } = hanging("still-out");
        let mut router = Router::new(loaded, Audit::new(&root.join("state"), MAX_BYTES));
        router.rules = Pool::new(1, 1);
        let loaded = router.loaded().expect("the router has its load");
        let hooked = [hook("begun/triage", None), hook("quiet/triage", None)];
        let base = Instant::now();
        let deadline = base
            .checked_add(Duration::from_secs(5))
            .unwrap_or_else(Instant::now);
        let clock = Clock::at(base);
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(Vec::new()),
            deadline,
            cancel: Cancel::new(Source::Harness),
        };
        let announced = |_hook: &Hook| clock.past(deadline);
        let reported = Mutex::new(Vec::new());
        let ran = |rule: &Rule, done: &RuleRun| {
            reported
                .lock()
                .unwrap()
                .push((rule.id().to_owned(), done.entry.clone()));
        };

        let returned = router.run_all(&loaded, &hooked, &ask, &|| clock.now(), &announced, &ran);
        drop(release);

        let deadline_line = json!({
            "rule": "begun/triage",
            "event": "sourcefed.event",
            "session": "s-1",
            "deadline": true,
        });
        let skipped = json!({
            "rule": "quiet/triage",
            "event": "sourcefed.event",
            "session": "s-1",
            "skipped": "deadline",
        });
        let returned: Vec<(String, bool)> = returned
            .iter()
            .map(|(rule, then)| (rule.id().to_owned(), then.is_some()))
            .collect();
        assert_eq!(
            returned,
            [
                ("begun/triage".to_owned(), false),
                ("quiet/triage".to_owned(), false),
            ],
            "each rule answers in hook order, with nothing to return"
        );
        assert_eq!(
            *reported.lock().unwrap(),
            [("begun/triage".to_owned(), deadline_line.clone())],
            "only the announced run settles a step"
        );
        assert_eq!(
            written(&root.join("state")),
            [deadline_line, skipped],
            "the log keeps hook order: the deadline, then the skipped rule"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A rule with a run still out is not started again: the second run
    /// returns before announcing and is logged as cooling, and once the first
    /// ends the rule runs again. The first run waits on a key the test never
    /// sends, so it is out until the test drops the sender.
    #[test]
    fn a_rule_with_a_run_out_is_not_started_again() {
        let Hanging {
            root,
            loaded,
            release,
            waiting,
        } = hanging("in-flight");
        let loaded = Arc::new(loaded);
        let cooldowns = Arc::new(Cooldowns::new());
        let flights = Arc::new(Flights::new());
        let rule = hook("in-flight/triage", None).rule;
        let ask = Ask {
            event: "sourcefed.event".to_owned(),
            session: "s-1".to_owned(),
            evidence: Arc::new(Vec::new()),
            deadline: Instant::now()
                .checked_add(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            cancel: Cancel::new(Source::Harness),
        };
        let (started, began) = mpsc::channel();
        let (loading, asking) = (Arc::clone(&loaded), ask.clone());
        let (cooling, flying, running) = (
            Arc::clone(&cooldowns),
            Arc::clone(&flights),
            Arc::clone(&rule),
        );
        let first = thread::spawn(move || {
            let announce = || {
                let _ = started.send(());
            };
            Router::run_one(
                &loading, &cooling, &flying, &running, None, &asking, &announce,
            )
        });
        began.recv().expect("the first run starts");
        waiting
            .recv_timeout(Duration::from_secs(5))
            .expect("the first run reaches its key");

        let second = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &|| {});

        assert_eq!(
            second.entry,
            json!({
                "rule": "in-flight/triage",
                "event": "sourcefed.event",
                "session": "s-1",
                "cooldown": true,
            }),
            "a run still out is logged as cooling"
        );
        assert!(second.then.is_none());

        drop(release);
        let first = first.join().expect("the first run ends");
        assert!(
            first.entry.get("cooldown").is_none(),
            "the first run starts: {}",
            first.entry
        );

        let third = Router::run_one(&loaded, &cooldowns, &flights, &rule, None, &ask, &|| {});

        assert!(
            third.entry.get("cooldown").is_none(),
            "the rule runs again once its run ends: {}",
            third.entry
        );
        let _ = fs::remove_dir_all(&root);
    }
}
