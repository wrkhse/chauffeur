use std::fmt;

use super::MAX_STEPS;

/// Why a plan was refused before any judge was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// A plan needs at least one step.
    Empty,
    /// A plan has at most [`MAX_STEPS`] steps; it has this many.
    TooManySteps(usize),
    /// Two steps share this ID.
    DuplicateStep(String),
    /// A step names a judge the panel lacks.
    UnknownJudge {
        /// The step's ID.
        step: String,
        /// The judge it names.
        judge: String,
    },
    /// A step waits on a step the plan lacks.
    UnknownNeed {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step waits on itself or a later step.
    LaterNeed {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step's need has an empty `when`.
    EmptyWhen {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step's object need names neither a bar nor a `when`.
    EmptyNeed {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step's need has a bar or margin over 1000 thousandths.
    OverOne {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step's need names an outcome its step does not have.
    NoSuchOutcome {
        /// The step's ID.
        step: String,
        /// The ID it waits on.
        need: String,
    },
    /// A step has no outcomes, or more outcomes and near needs than the
    /// step machine's codes hold.
    Outcomes {
        /// The step's ID.
        step: String,
    },
    /// The step machine refused the plan, for this reason.
    Machine(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a plan needs at least one step"),
            Self::TooManySteps(count) => {
                write!(f, "a plan has at most {MAX_STEPS} steps, not {count}")
            }
            Self::DuplicateStep(id) => write!(f, "two steps are named `{id}`"),
            Self::UnknownJudge { step, judge } => {
                write!(
                    f,
                    "step `{step}` names judge `{judge}`, which the panel lacks"
                )
            }
            Self::UnknownNeed { step, need } => {
                write!(f, "step `{step}` waits on `{need}`, which the plan lacks")
            }
            Self::LaterNeed { step, need } => write!(
                f,
                "step `{step}` waits on `{need}`, which is not an earlier step"
            ),
            Self::EmptyWhen { step, need } => {
                write!(f, "step `{step}` waits on `{need}` with an empty `when`")
            }
            Self::EmptyNeed { step, need } => write!(
                f,
                "step `{step}` waits on `{need}` with neither a bar nor a `when`"
            ),
            Self::OverOne { step, need } => write!(
                f,
                "step `{step}` waits on `{need}` with a bar or margin over 1"
            ),
            Self::NoSuchOutcome { step, need } => write!(
                f,
                "step `{step}` waits on an outcome `{need}` does not have"
            ),
            Self::Outcomes { step } => write!(
                f,
                "step `{step}` has no outcomes, or more outcomes and near needs than its codes hold"
            ),
            Self::Machine(reason) => write!(f, "the step machine refused the plan: {reason}"),
        }
    }
}

impl std::error::Error for PlanError {}
