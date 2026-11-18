use std::fmt;

use super::milli::Milli;
use super::probabilities::Probabilities;
use super::{Confidence, pick};
use crate::question::Question;

/// What an answer settled to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// One of the question's outcomes.
    Outcome(String),
    /// No outcome was likely enough.
    Unsure,
}

impl fmt::Display for Settled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Outcome(value) => f.write_str(value),
            Self::Unsure => f.write_str("unsure"),
        }
    }
}

/// The model that gave an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsweredBy {
    /// Its name in `models.json`.
    pub model: String,
    /// Whether it is a placeholder, standing in for a real model.
    pub placeholder: bool,
}

/// A judge's answer: what it settled to, every outcome's probability, its
/// level, and the model that gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    outcome: Settled,
    p: Vec<(String, Milli)>,
    level: f64,
    by: AnsweredBy,
}

impl Answer {
    /// Settles `p`, given by `by`, for `question` under `confidence`.
    #[must_use]
    pub fn settle(
        question: &Question,
        p: Probabilities,
        confidence: &Confidence,
        by: AnsweredBy,
    ) -> Self {
        let outcomes = question.outcomes();
        let picked = pick(p.as_slice(), |index| confidence.bar(outcomes, index));
        let outcome = picked
            .and_then(|index| outcomes.get(index))
            .map_or(Settled::Unsure, |label| {
                Settled::Outcome(label.value().to_owned())
            });
        let level = p.level();
        let p = outcomes
            .iter()
            .map(|label| label.value().to_owned())
            .zip(p.0)
            .collect();
        Self {
            outcome,
            p,
            level,
            by,
        }
    }

    /// The model that gave it.
    #[must_use]
    pub const fn by(&self) -> &AnsweredBy {
        &self.by
    }

    /// What the answer settled to.
    #[must_use]
    pub const fn outcome(&self) -> &Settled {
        &self.outcome
    }

    /// Every outcome's probability, in the question's order.
    pub fn p(&self) -> impl Iterator<Item = (&str, Milli)> {
        self.p.iter().map(|(value, p)| (value.as_str(), *p))
    }

    /// The probability-weighted position in the outcome list.
    #[must_use]
    pub const fn level(&self) -> f64 {
        self.level
    }
}

impl chauffeur_core::Reply for Answer {
    /// The settled outcome's position in the question's order, or `unsure`'s,
    /// which follows every outcome.
    fn settled(&self) -> u8 {
        let index = match &self.outcome {
            Settled::Outcome(label) => self
                .p
                .iter()
                .position(|(value, _)| value == label)
                .unwrap_or(self.p.len()),
            Settled::Unsure => self.p.len(),
        };
        u8::try_from(index).unwrap_or(u8::MAX)
    }

    fn p(&self, outcome: u8) -> Option<u16> {
        self.p.get(usize::from(outcome)).map(|(_, p)| p.get())
    }
}

#[cfg(test)]
mod tests {
    use chauffeur_core::Reply;

    use super::{Answer, AnsweredBy, Settled};
    use crate::confidence::milli::Milli;
    use crate::confidence::{Confidence, Probabilities};
    use crate::question::Question;

    fn answer(yes: f64) -> Answer {
        let question = Question::noul("Ship it?", ["diff"]).unwrap();
        let p =
            Probabilities::read(question.outcomes(), [("yes", yes), ("no", 1.0 - yes)]).unwrap();
        let bar = Confidence::Bar(Milli::new(700).unwrap());
        let by = AnsweredBy {
            model: "jev".into(),
            placeholder: false,
        };
        Answer::settle(&question, p, &bar, by)
    }

    #[test]
    fn an_answer_settles_to_its_outcome_position_or_unsure_after_them() {
        let no = answer(0.1);
        let unsure = answer(0.5);

        assert_eq!(no.outcome(), &Settled::Outcome("no".into()));
        assert_eq!(no.settled(), 1);
        assert_eq!(unsure.outcome(), &Settled::Unsure);
        assert_eq!(unsure.settled(), 2);
    }

    #[test]
    fn its_p_is_each_outcome_in_thousandths() {
        let no = answer(0.1);

        assert_eq!(Reply::p(&no, 0), Some(100));
        assert_eq!(Reply::p(&no, 1), Some(900));
        assert_eq!(Reply::p(&no, 5), None);
    }
}
