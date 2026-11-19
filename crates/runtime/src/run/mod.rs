mod log;
mod pool;
mod rounds;
mod wait;

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chauffeur_core::{Outcome, Panel, PlanError, Round, Run, Step};
use chauffeur_judges::{Answer, Evidence, Fallback, Models, NamedJudge, Question};
use chauffeur_protocol::{Event, JudgeSpec, Rule, Strategy, Then};

use crate::Source;
use crate::redact::{Masking, Redactor};

pub use chauffeur_judges::Rests;
pub use log::{EventLog, MAX_EVENTS};
pub use pool::{ExecuteError, Job, Pool};
pub use wait::{Cancel, Waited, within};

/// The most workers kept waiting between runs: enough for every rule hooked
/// by one event to start at once.
const KEEP: usize = 8;

/// The most workers alive in each source's pool. Each pool is twice the daemon's
/// whole capacity, so runs that outlive their deadline, still waiting on their
/// models, leave room for new ones, and integration work never occupies harness
/// workers.
const MAX_WORKERS: usize = 32;

/// The workers every run in this process shares.
static HARNESS_WORKERS: LazyLock<Pool> = LazyLock::new(|| Pool::new(KEEP, MAX_WORKERS));
static INTEGRATION_WORKERS: LazyLock<Pool> = LazyLock::new(|| Pool::new(KEEP, MAX_WORKERS));

/// Finds a model's key by its environment variable's name.
pub type Keys = dyn Fn(&str) -> Option<String> + Send + Sync;

/// What a rule needs to run: the models, the redactor and the keys.
pub struct Context {
    /// Registered models and tiers.
    pub models: Models,
    /// Redacts evidence before any judge sees it.
    pub redactor: Redactor,
    /// Finds model keys.
    pub keys: Box<Keys>,
    /// Shared model-rest state, keyed by model name. A model that cannot
    /// answer rests here and is skipped while it rests; the registry
    /// survives across runs while this `Context` lives.
    pub rests: Rests,
}

/// How a strategy's run ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum Ran {
    /// Every step settled: the run's events and verdict.
    Settled(Run<Answer>),
    /// The strategy could not run: a placeholder whose answers would not
    /// load, or a worker whose thread could not start.
    Refused(String),
    /// Every worker was busy.
    Busy,
    /// The run's worker stopped without a result.
    Stopped,
    /// The deadline passed first; a late result is dropped.
    Deadline,
    /// The caller cancelled first; a late result is dropped.
    Cancelled,
}

impl Ran {
    /// The first `then` entry of `rule` its verdict meets, if it settled.
    #[must_use]
    pub fn then<'a>(&self, rule: &'a Rule) -> Option<&'a Then> {
        match self {
            Self::Settled(run) => Then::first_met(rule.then(), &run.verdict),
            Self::Refused(_) | Self::Busy | Self::Stopped | Self::Deadline | Self::Cancelled => {
                None
            }
        }
    }
}

/// Runs `rule` for `event`, waiting at most `deadline`.
#[must_use]
pub fn run(context: &Arc<Context>, rule: &Arc<Rule>, event: &Event, deadline: Duration) -> Ran {
    let deadline = Instant::now().checked_add(deadline);
    let Some(deadline) = deadline else {
        return Ran::Refused("the deadline is too far away".to_owned());
    };
    run_strategy(
        context,
        rule,
        &event.evidence(),
        deadline,
        &Cancel::new(Source::Harness),
    )
}

/// Runs `strategy` over `evidence` on a pooled worker thread, until
/// `deadline` or `cancel`. The evidence is redacted here, before any judge
/// sees it. The worker is free again within its models' timeouts.
#[must_use]
pub fn run_strategy<S: AsRef<Strategy> + Send + Sync + 'static>(
    context: &Arc<Context>,
    strategy: &Arc<S>,
    evidence: &[(String, String)],
    deadline: Instant,
    cancel: &Cancel,
) -> Ran {
    let mut masking = Masking::default();
    let blocks = evidence
        .iter()
        .map(|(name, content)| (name, context.redactor.redact(content, &mut masking)));
    let evidence = Evidence::new(blocks);
    let (context, strategy) = (Arc::clone(context), Arc::clone(strategy));
    let job = move |deadline: Instant, cancel: &Cancel| {
        run_now(&context, (*strategy).as_ref(), &evidence, deadline, cancel)
    };
    let workers = match cancel.source() {
        Source::Harness => &HARNESS_WORKERS,
        Source::Integration => &INTEGRATION_WORKERS,
    };
    match within(workers, deadline, cancel, job) {
        Waited::Done(ran) => ran,
        Waited::Deadline => Ran::Deadline,
        Waited::Cancelled => Ran::Cancelled,
        Waited::Busy => Ran::Busy,
        Waited::Failed(error) => Ran::Refused(format!("a worker could not start: {error}")),
        Waited::Stopped => Ran::Stopped,
    }
}

fn run_now(
    context: &Context,
    strategy: &Strategy,
    evidence: &Evidence,
    deadline: Instant,
    cancel: &Cancel,
) -> Ran {
    // Checked before the round: the caller may have gone, or waited too long.
    if cancel.is_cancelled() {
        return Ran::Cancelled;
    }
    if Instant::now() >= deadline {
        return Ran::Deadline;
    }

    let running = strategy.plan().start(Some(deadline));
    let mut fallbacks = Vec::with_capacity(strategy.judges().len());

    for (name, spec) in strategy.judges() {
        match Fallback::build(
            &context.models,
            spec.tier,
            spec.model.as_deref(),
            &context.keys,
        )
        .map(|fallback| fallback.with_rests(context.rests.clone()))
        {
            Ok(fallback) => fallbacks.push((name, spec, fallback)),
            Err(error) => return Ran::Refused(error.to_string()),
        }
    }

    let missing = |step: &&Step<Question>| {
        !fallbacks
            .iter()
            .any(|(name, ..)| name.as_str() == step.judge())
    };

    if let Some(step) = strategy.plan().steps().iter().find(missing) {
        return Ran::Refused(
            PlanError::UnknownJudge {
                step: step.id().to_owned(),
                judge: step.judge().to_owned(),
            }
            .to_string(),
        );
    }

    let ask = |round: &Round<'_, Question>, end: Option<Instant>| {
        ask_until(&fallbacks, evidence, round, end)
    };

    rounds::run(running, &ask, deadline, cancel)
}

/// Asks `round` of the strategy's judges, their model work ending at `end`.
/// The panel is built for this round alone, so each round's end is the one
/// its judges hold.
fn ask_until(
    fallbacks: &[(&String, &JudgeSpec, Fallback)],
    evidence: &Evidence,
    round: &Round<'_, Question>,
    end: Option<Instant>,
) -> Vec<(String, Outcome<Answer>)> {
    let judges: Vec<(&str, NamedJudge<'_>)> = fallbacks
        .iter()
        .map(|(name, spec, fallback)| {
            let judge = NamedJudge::new(fallback, &spec.confidence, evidence);

            (name.as_str(), judge.with_deadline(end))
        })
        .collect();
    let panel = judges.iter().fold(Panel::new(), |panel, (name, judge)| {
        panel.with(*name, judge)
    });

    round.ask(&panel)
}
