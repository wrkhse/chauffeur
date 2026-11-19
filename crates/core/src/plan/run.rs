use std::fmt;
use std::time::{Duration, Instant};

use rungs::Status;

use super::near::{self, encode};
use super::{Failure, Outcome, Plan, Reply, Step};
use crate::event::{Event, Run, Verdict};
use crate::judge::Panel;

/// Kept back before the caller's deadline, so the answers in hand return in time.
pub const RESERVE: Duration = Duration::from_secs(1);

/// A round after the first starts only with this much left before the end.
pub const MIN_ESCALATION: Duration = Duration::from_millis(500);

/// A plan being run, round by round. The caller asks each round's steps,
/// passes their answers back, and passes a clock, so core keeps none.
pub struct Running<'p, Q, O> {
    plan: &'p Plan<Q>,
    run: rungs::Run<'p>,
    end: Option<Instant>,
    round: u32,
    open: Vec<usize>,
    opened: Vec<Option<u32>>,
    outcomes: Vec<Option<Outcome<O>>>,
    skipped: Vec<bool>,
    events: Vec<Event<O>>,
}

impl<Q> Plan<Q> {
    /// Starts a run that should be over by `deadline`.
    #[must_use]
    pub fn start<O>(&self, deadline: Option<Instant>) -> Running<'_, Q, O> {
        let ids = self.steps.iter().map(|step| step.id().to_owned()).collect();

        Running {
            plan: self,
            run: self.machine.start(),
            end: deadline.map(|deadline| deadline.checked_sub(RESERVE).unwrap_or(deadline)),
            round: 0,
            open: Vec::new(),
            opened: vec![None; self.steps.len()],
            outcomes: std::iter::repeat_with(|| None)
                .take(self.steps.len())
                .collect(),
            skipped: vec![false; self.steps.len()],
            events: vec![Event::Started { steps: ids }],
        }
    }
}

impl<'p, Q, O: Reply + Clone> Running<'p, Q, O> {
    /// When the steps end: [`RESERVE`] before the deadline, or the deadline
    /// itself when that is too early to subtract from.
    #[must_use]
    pub const fn end(&self) -> Option<Instant> {
        self.end
    }

    /// The round to ask now, or none when the run is over. While a round is
    /// open it is returned again, unchanged.
    ///
    /// `clock` gives the current time. It is called only for a run with a
    /// deadline, before a round after the first while a step is pending, so
    /// a run without one never reads it.
    pub fn questions(&mut self, clock: impl FnOnce() -> Instant) -> Option<Round<'p, Q>> {
        if !self.open.is_empty() {
            return Some(self.round_of_open());
        }

        let before: Vec<Option<Status>> = self.statuses();
        let pending = before.contains(&Some(Status::Pending));
        let late = self.round > 0
            && pending
            && self
                .end
                .is_some_and(|end| end.saturating_duration_since(clock()) < MIN_ESCALATION);
        if late {
            for (index, status) in before.iter().enumerate() {
                if *status == Some(Status::Pending) {
                    let _skipped = self.run.skip(index);
                }
            }
        }

        let started = self.run.advance();
        self.record_skips(&before);
        if started.is_empty() {
            return None;
        }

        self.round = self.round.saturating_add(1);
        self.open = started.iter().collect();

        for index in &self.open {
            if let Some(opened) = self.opened.get_mut(*index) {
                *opened = Some(self.round);
            }
        }

        self.events.push(Event::RoundReady {
            round: self.round,
            steps: self.open_ids(),
        });

        Some(self.round_of_open())
    }

    /// Settles every step of the round at once. All or nothing: on an error
    /// the run is unchanged.
    ///
    /// # Errors
    ///
    /// [`ReplyError::NotAsked`] for a step not open in this round,
    /// [`ReplyError::Repeated`] for a step answered twice,
    /// [`ReplyError::Missing`] for an open step left out, and
    /// [`ReplyError::Refused`] when the step machine refuses the answers.
    pub fn answer(
        &mut self,
        reply: impl IntoIterator<Item = (String, Outcome<O>)>,
    ) -> Result<(), ReplyError> {
        let mut slots: Vec<Option<Outcome<O>>> = self.open.iter().map(|_| None).collect();
        for (id, outcome) in reply {
            let slot = self
                .open
                .iter()
                .position(|index| self.id(*index) == Some(id.as_str()))
                .and_then(|position| slots.get_mut(position));
            match slot {
                None => return Err(ReplyError::NotAsked(id)),
                Some(Some(_)) => return Err(ReplyError::Repeated(id)),
                Some(slot) => *slot = Some(outcome),
            }
        }

        let mut batch = Vec::with_capacity(slots.len());
        let mut kept = Vec::with_capacity(slots.len());
        for (index, slot) in self.open.iter().zip(slots) {
            let Some(outcome) = slot else {
                let id = self.id(*index).unwrap_or_default().to_owned();
                return Err(ReplyError::Missing(id));
            };
            let (settle, outcome) = self.lower(*index, outcome);

            batch.push((*index, settle));
            kept.push((*index, outcome));
        }

        self.run = settle_batch(&self.run, &batch)
            .map_err(|error| ReplyError::Refused(error.to_string()))?;
        for (index, outcome) in kept {
            self.record(index, outcome);
        }
        self.open.clear();
        Ok(())
    }

    /// Settles one step of the round as failed.
    ///
    /// # Errors
    ///
    /// [`ReplyError::NotAsked`] for a step not open in this round, and
    /// [`ReplyError::Refused`] when the step machine refuses it.
    pub fn fail(&mut self, step: &str, failure: Failure) -> Result<(), ReplyError> {
        let Some(position) = self
            .open
            .iter()
            .position(|index| self.id(*index) == Some(step))
        else {
            return Err(ReplyError::NotAsked(step.to_owned()));
        };
        let index = self.open.remove(position);

        if let Err(error) = self.run.fail(index) {
            self.open.insert(position, index);
            return Err(ReplyError::Refused(error.to_string()));
        }
        self.record(index, Outcome::Failed(failure));
        Ok(())
    }

    /// Settles every step still open in the round as failed with `failure`,
    /// such as when the round's answers were refused.
    pub fn fail_round(&mut self, failure: Failure) {
        for index in std::mem::take(&mut self.open) {
            let _failed = self.run.fail(index);
            self.record(index, Outcome::Failed(failure));
        }
    }

    /// The events and the verdict. A step still open settles as failed by
    /// the deadline; a step never decided is skipped.
    #[must_use]
    pub fn finish(mut self) -> Run<O> {
        self.fail_round(Failure::Deadline);

        for index in 0..self.plan.steps.len() {
            if self.run.status(index) == Some(Status::Pending) {
                let _skipped = self.run.skip(index);
                self.skip(index);
            }
        }
        self.events.push(Event::Finished);

        let decision = self
            .opened
            .iter()
            .enumerate()
            .filter_map(|(index, round)| round.map(|round| (round, index)))
            .max()
            .and_then(|(_, index)| self.id(index))
            .map(str::to_owned);
        let ids = self.plan.steps.iter().map(|step| step.id().to_owned());
        let settled = ids
            .clone()
            .zip(self.outcomes)
            .filter_map(|(id, outcome)| outcome.map(|outcome| (id, outcome)))
            .collect();
        let skipped = ids
            .zip(self.skipped)
            .filter_map(|(id, skipped)| skipped.then_some(id))
            .collect();

        Run {
            events: self.events,
            verdict: Verdict::new(settled, skipped, decision),
        }
    }

    /// What `outcome` does to step `index` on the machine, and the outcome
    /// it keeps: an answer outside its values, or one whose code does not
    /// fit, is invalid.
    fn lower(&self, index: usize, outcome: Outcome<O>) -> (Settle, Outcome<O>) {
        match outcome {
            Outcome::Value(reply) => self
                .code(index, &reply)
                .map_or((Settle::Fail, Outcome::Failed(Failure::Invalid)), |code| {
                    (Settle::Code(code), Outcome::Value(reply))
                }),
            failed @ Outcome::Failed(_) => (Settle::Fail, failed),
        }
    }

    /// The machine code of `reply` to step `index`: its value, and which of
    /// the near needs reading it match.
    fn code(&self, index: usize, reply: &O) -> Option<rungs::Outcome> {
        let shape = self.plan.shapes.get(index)?;
        let settled = reply.settled();
        if settled >= shape.outcomes {
            return None;
        }

        let mut matched = 0_u8;
        for (bit, near) in shape.nears.iter().enumerate() {
            if near::matches(near, reply) {
                matched |= 1_u8.checked_shl(u32::try_from(bit).ok()?)?;
            }
        }

        encode(settled, matched, shape.k())
    }

    fn record(&mut self, index: usize, outcome: Outcome<O>) {
        let Some(id) = self.id(index).map(str::to_owned) else {
            return;
        };
        if let Some(slot) = self.outcomes.get_mut(index) {
            *slot = Some(outcome.clone());
        }
        self.events.push(Event::StepSettled { step: id, outcome });
    }
}

impl<'p, Q, O> Running<'p, Q, O> {
    fn id(&self, index: usize) -> Option<&'p str> {
        self.plan.steps.get(index).map(Step::id)
    }

    fn statuses(&self) -> Vec<Option<Status>> {
        (0..self.plan.steps.len())
            .map(|index| self.run.status(index))
            .collect()
    }

    /// Records every step that went from Pending to Skipped since `before`.
    fn record_skips(&mut self, before: &[Option<Status>]) {
        for (index, status) in before.iter().enumerate() {
            let now = self.run.status(index);
            if *status == Some(Status::Pending) && now == Some(Status::Skipped) {
                self.skip(index);
            }
        }
    }

    fn skip(&mut self, index: usize) {
        let Some(id) = self.id(index).map(str::to_owned) else {
            return;
        };
        if let Some(slot) = self.skipped.get_mut(index) {
            *slot = true;
        }
        self.events.push(Event::StepSkipped { step: id });
    }

    fn open_ids(&self) -> Vec<String> {
        self.open
            .iter()
            .filter_map(|index| self.id(*index).map(str::to_owned))
            .collect()
    }

    fn round_of_open(&self) -> Round<'p, Q> {
        let plan = self.plan;
        Round {
            number: self.round,
            steps: self
                .open
                .iter()
                .filter_map(|index| plan.steps.get(*index))
                .collect(),
        }
    }
}

/// What a reply does to one step on the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// The step settles with this code.
    Code(rungs::Outcome),
    /// The step fails.
    Fail,
}

/// Applies every item to a copy of `run` and returns the copy, or the first
/// refusal; `run` itself is never changed, so a batch settles all of its
/// steps or none.
///
/// # Errors
///
/// The machine's refusal of the first item it refuses: a step past the plan
/// or not running, or a code the step lacks.
pub fn settle_batch<'p>(
    run: &rungs::Run<'p>,
    batch: &[(usize, Settle)],
) -> Result<rungs::Run<'p>, rungs::RunError> {
    let mut next = *run;
    for (index, settle) in batch {
        match settle {
            Settle::Code(code) => next.settle(*index, *code)?,
            Settle::Fail => {
                next.fail(*index)?;
            }
        }
    }

    Ok(next)
}

/// One round's steps, to ask of their judges.
pub struct Round<'p, Q> {
    number: u32,
    steps: Vec<&'p Step<Q>>,
}

impl<'p, Q> Round<'p, Q> {
    /// The round's number, from 1.
    #[must_use]
    pub const fn number(&self) -> u32 {
        self.number
    }

    /// The steps to ask, in plan order.
    #[must_use]
    pub fn steps(&self) -> &[&'p Step<Q>] {
        &self.steps
    }

    /// Asks each judge its share of the round in one `judge_all` call,
    /// judges in the order they first appear. A batch of the wrong length
    /// fails each of its steps as invalid; a judge missing from the panel
    /// fails its steps as unavailable.
    #[must_use]
    pub fn ask<O>(&self, panel: &Panel<'_, Q, O>) -> Vec<(String, Outcome<O>)> {
        let mut shares: Vec<(&str, Vec<&'p Step<Q>>)> = Vec::new();
        for step in &self.steps {
            match shares.iter_mut().find(|(name, _)| *name == step.judge()) {
                Some((_, steps)) => steps.push(step),
                None => shares.push((step.judge(), vec![step])),
            }
        }

        let mut answers = Vec::with_capacity(self.steps.len());
        for (name, steps) in shares {
            let requests: Vec<&Q> = steps.iter().map(|step| step.request()).collect();
            let outcomes = panel.get(name).map_or_else(
                || failed(steps.len(), Failure::Unavailable),
                |judge| {
                    let outcomes = judge.judge_all(&requests);
                    if outcomes.len() == steps.len() {
                        outcomes
                    } else {
                        failed(steps.len(), Failure::Invalid)
                    }
                },
            );
            answers.extend(steps.iter().map(|step| step.id().to_owned()).zip(outcomes));
        }

        answers
    }
}

fn failed<O>(count: usize, failure: Failure) -> Vec<Outcome<O>> {
    std::iter::repeat_with(|| Outcome::Failed(failure))
        .take(count)
        .collect()
}

/// Why a round's answers were refused; the run is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyError {
    /// This step is not open in the round.
    NotAsked(String),
    /// This step was answered twice.
    Repeated(String),
    /// This open step was not answered.
    Missing(String),
    /// The step machine refused the answers, for this reason.
    Refused(String),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAsked(step) => write!(f, "step `{step}` was not asked in this round"),
            Self::Repeated(step) => write!(f, "step `{step}` was answered twice"),
            Self::Missing(step) => write!(f, "step `{step}` was asked but not answered"),
            Self::Refused(reason) => write!(f, "the step machine refused the answers: {reason}"),
        }
    }
}

impl std::error::Error for ReplyError {}
