/// What a step's after waits for from one earlier step, named by its ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    step: String,
    wants: Wants,
}

impl Need {
    /// The step named `step` finished: settled, failed or skipped.
    pub fn settled(step: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            wants: Wants::Settled,
        }
    }

    /// The step named `step` settled with an answer near one of `near`'s
    /// bars, or one of `when`.
    pub fn outcome(step: impl Into<String>, near: Option<Near>, when: Option<Vec<When>>) -> Self {
        Self {
            step: step.into(),
            wants: Wants::Outcome { near, when },
        }
    }

    /// The ID of the step it waits on.
    #[must_use]
    pub fn step(&self) -> &str {
        &self.step
    }

    /// What it waits for.
    #[must_use]
    pub const fn wants(&self) -> &Wants {
        &self.wants
    }
}

/// What a need waits for from its step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wants {
    /// The step finished: settled, failed or skipped. A rule's string need.
    Settled,
    /// The step settled with a matching answer: near one of its bars, or one
    /// of `when`. A rule's object need. It never matches a skipped step.
    Outcome {
        /// Bars the answer's probability must lie near.
        near: Option<Near>,
        /// Values or a failure the step must settle to.
        when: Option<Vec<When>>,
    },
}

/// One way a step can settle, as a need names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum When {
    /// The answer settled to the value at this index.
    Outcome(u8),
    /// The step failed.
    Failed,
}

/// Matches when the answer's probability for one of `bars`' outcomes lies
/// within `margin` of its bar, inclusive. Thousandths keep the boundaries
/// exact: 850 is within 150 of 700.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Near {
    margin: u16,
    bars: Vec<Bar>,
}

impl Near {
    /// Bars `bars`, each matched within `margin` thousandths.
    pub fn new(margin: u16, bars: impl IntoIterator<Item = Bar>) -> Self {
        Self {
            margin,
            bars: bars.into_iter().collect(),
        }
    }

    /// How far from a bar still matches, in thousandths.
    #[must_use]
    pub const fn margin(&self) -> u16 {
        self.margin
    }

    /// The bars.
    #[must_use]
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }
}

/// A bar on one outcome's probability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bar {
    /// The outcome's index.
    pub outcome: u8,
    /// The bar, in thousandths.
    pub at: u16,
}
