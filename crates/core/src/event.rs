use crate::plan::{DECISION, Outcome};

/// What happened while a plan ran, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<O> {
    /// The run began, with every step's ID in plan order.
    Started {
        /// The plan's step IDs.
        steps: Vec<String>,
    },
    /// A round's steps were asked of their judges.
    RoundReady {
        /// The round's number, from 1.
        round: u32,
        /// The steps asked in it.
        steps: Vec<String>,
    },
    /// A step's judge answered or failed.
    StepSettled {
        /// The step's ID.
        step: String,
        /// Its outcome.
        outcome: Outcome<O>,
    },
    /// A step was decided against: its needs could not hold, or too little
    /// time was left.
    StepSkipped {
        /// The step's ID.
        step: String,
    },
    /// The run is over.
    Finished,
}

/// Every settled step's outcome, and every skipped step, in plan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict<O> {
    settled: Vec<(String, Outcome<O>)>,
    skipped: Vec<String>,
    decision: Option<String>,
}

impl<O> Verdict<O> {
    pub(crate) const fn new(
        settled: Vec<(String, Outcome<O>)>,
        skipped: Vec<String>,
        decision: Option<String>,
    ) -> Self {
        Self {
            settled,
            skipped,
            decision,
        }
    }

    /// The named outcome, or for [`DECISION`] the last opened step by round then
    /// plan index (including failures), with none for skipped or unopened steps.
    #[must_use]
    pub fn get(&self, step: &str) -> Option<&Outcome<O>> {
        let step = if step == DECISION {
            self.decision.as_deref()?
        } else {
            step
        };

        self.settled
            .iter()
            .find(|(id, _)| id == step)
            .map(|(_, outcome)| outcome)
    }

    /// The last opened step by round then plan index and its outcome (including
    /// failures), or none if no step opened, regardless of settlement order.
    #[must_use]
    pub fn decision(&self) -> Option<(&str, &Outcome<O>)> {
        let step = self.decision.as_deref()?;

        self.settled
            .iter()
            .find(|(id, _)| id == step)
            .map(|(id, outcome)| (id.as_str(), outcome))
    }

    /// The IDs of the skipped steps, in plan order.
    pub fn skipped(&self) -> impl Iterator<Item = &str> {
        self.skipped.iter().map(String::as_str)
    }

    /// Each settled step's ID and outcome, in plan order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Outcome<O>)> {
        self.settled
            .iter()
            .map(|(id, outcome)| (id.as_str(), outcome))
    }
}

/// A finished run: its events, and the verdict they end in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run<O> {
    /// Every event, in order.
    pub events: Vec<Event<O>>,
    /// Every step's outcome.
    pub verdict: Verdict<O>,
}
