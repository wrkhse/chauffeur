mod build;
mod error;
pub mod near;
mod need;
mod outcome;
mod reply;
pub mod run;
mod step;

pub use error::PlanError;
pub use need::{Bar, Near, Need, Wants, When};
pub use outcome::{Failure, Outcome};
pub use reply::{Reply, Request};
pub use run::{MIN_ESCALATION, RESERVE, ReplyError, Round, Running};
pub use step::Step;

use std::time::Instant;

use crate::event::Run;
use crate::judge::Panel;

/// The most steps a plan has, as many as the step machine holds.
pub const MAX_STEPS: usize = rungs::MAX_STEPS;

/// The virtual step naming the last opened step's outcome in a verdict.
pub const DECISION: &str = "decision";

/// An ordered list of uniquely named steps, each waiting only on earlier
/// ones. Each round asks every step whose needs have come to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan<Q> {
    steps: Vec<Step<Q>>,
    // Boxed: the machine is a fixed array of its most steps, kilobytes big.
    machine: Box<rungs::Plan>,
    shapes: Vec<Shape>,
}

/// What the machine reads of one step: its values and the near needs that
/// read its answers, in bit order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shape {
    outcomes: u8,
    nears: Vec<Near>,
}

impl Shape {
    /// How many near needs read the step's answers.
    fn k(&self) -> u8 {
        u8::try_from(self.nears.len()).unwrap_or(u8::MAX)
    }
}

impl<Q: Request> Plan<Q> {
    /// A plan of `steps`, in order.
    ///
    /// # Errors
    ///
    /// [`PlanError::Empty`] with no steps, [`PlanError::TooManySteps`] past
    /// [`MAX_STEPS`], [`PlanError::DuplicateStep`] when two steps share an
    /// ID, [`PlanError::ReservedStep`] for the [`DECISION`] ID,
    /// a need error when a step waits on what no earlier step can give,
    /// and [`PlanError::Outcomes`] when a step's codes do not fit.
    pub fn new(steps: impl IntoIterator<Item = Step<Q>>) -> Result<Self, PlanError> {
        let steps: Vec<Step<Q>> = steps.into_iter().collect();
        build::check_count(&steps)?;
        build::check_ids(&steps)?;

        let targets = build::check_needs(&steps)?;
        let (shapes, bits) = build::shapes(&steps, &targets)?;
        let machine = build::machine(&steps, &targets, &bits, &shapes)?;

        Ok(Self {
            steps,
            machine: Box::new(machine),
            shapes,
        })
    }
}

impl<Q> Plan<Q> {
    /// The steps, in order.
    #[must_use]
    pub fn steps(&self) -> &[Step<Q>] {
        &self.steps
    }

    /// Asks every step's judge, round by round, and returns the events and
    /// the verdict. Each judge answers its share of a round in one
    /// `judge_all` call, in the order the judges first appear in it.
    ///
    /// # Errors
    ///
    /// [`PlanError::UnknownJudge`] when a step names a judge the panel lacks;
    /// no judge is asked.
    pub fn run<O: Reply + Clone>(&self, panel: &Panel<'_, Q, O>) -> Result<Run<O>, PlanError> {
        if let Some(step) = self
            .steps
            .iter()
            .find(|step| panel.get(step.judge()).is_none())
        {
            return Err(PlanError::UnknownJudge {
                step: step.id().to_owned(),
                judge: step.judge().to_owned(),
            });
        }

        let mut running = self.start(None);
        while let Some(round) = running.questions(Instant::now) {
            let reply = round.ask(panel);

            // Round::ask answers every open step once, so a refused reply
            // means a judge broke the machine's rules: its steps fail as invalid.
            if running.answer(reply).is_err() {
                running.fail_round(Failure::Invalid);
            }
        }

        Ok(running.finish())
    }
}
