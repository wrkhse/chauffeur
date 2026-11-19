use std::collections::BTreeMap;
use std::fmt::Write as _;

use chauffeur_core::Failure;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SystemOneAsk;
use crate::confidence::Probabilities;
use crate::format::Evidence;
use crate::question::{Label, Outcomes, Question};

/// A `/v1/systemone` request: a round's questions by ID, and the evidence
/// they name.
#[derive(Serialize)]
pub(super) struct Request<'a> {
    pub(super) model: &'a str,
    pub(super) state: String,
    pub(super) questions: BTreeMap<&'a str, &'a Asked<'a>>,
    /// Left out when the model's entry does not set it: a provider may refuse
    /// a top-level key it does not know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) store: Option<bool>,
}

/// One question as the API is asked it. Its `type` is also the `type` its
/// answer must carry.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum Asked<'a> {
    /// Yes or no. The answer is the chance of `yes`.
    Noul { instructions: &'a str },
    /// One of the outcomes, each with what the model reads about it: its
    /// description, or its value when it has none.
    Choice {
        instructions: &'a str,
        criteria: BTreeMap<&'a str, &'a str>,
    },
    /// A level on a scale: the outcomes' values in order, lowest first.
    Score {
        instructions: &'a str,
        criteria: Vec<&'a str>,
    },
}

impl<'a> Asked<'a> {
    /// How `question` is asked: as `noul` when its outcomes are exactly `yes`
    /// and `no`, and as `ask` says otherwise.
    pub(super) fn of(question: &'a Question, ask: SystemOneAsk) -> Self {
        let instructions = question.instructions();
        let outcomes = question.outcomes();

        if is_noul(outcomes) {
            return Self::Noul { instructions };
        }

        match ask {
            SystemOneAsk::Choice => Self::Choice {
                instructions,
                criteria: outcomes
                    .iter()
                    .map(|label| {
                        let value = label.value();
                        (value, label.description().unwrap_or(value))
                    })
                    .collect(),
            },
            SystemOneAsk::Score => Self::Score {
                instructions,
                criteria: outcomes.iter().map(Label::value).collect(),
            },
        }
    }

    const fn kind(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }

    /// The probabilities in `answer`, the reply to this question, whose
    /// outcomes are `outcomes`. An answer that is missing, cannot be read, is
    /// of another type or reports a confidence outside 0 to 1 is invalid.
    pub(super) fn read(
        &self,
        outcomes: &Outcomes,
        answer: Option<&Value>,
    ) -> Result<Probabilities, Failure> {
        let answer = answer
            .and_then(|answer| Answered::deserialize(answer).ok())
            .ok_or(Failure::Invalid)?;
        let confident = answer.confidence.is_none_or(is_probability);

        if answer.kind != self.kind() || !confident {
            return Err(Failure::Invalid);
        }

        match self {
            Self::Noul { .. } => answer.noul(outcomes),
            Self::Choice { .. } => answer.choice(outcomes),
            Self::Score { .. } => answer.score(outcomes),
        }
    }
}

/// A `/v1/systemone` reply: an answer per question ID. Each answer is read
/// on its own, so one that cannot be read fails only its question.
#[derive(Deserialize)]
pub(super) struct Response {
    pub(super) answers: BTreeMap<String, Value>,
}

/// One answer as the API gives it. `legend` and any other field are not read.
#[derive(Deserialize)]
struct Answered {
    #[serde(rename = "type")]
    kind: String,
    noul: Option<f64>,
    choice: Option<String>,
    score: Option<f64>,
    expected_level: Option<f64>,
    confidence: Option<f64>,
    probabilities: Option<BTreeMap<String, f64>>,
}

impl Answered {
    /// A `noul` answer's probabilities, from the chance of `yes`.
    fn noul(&self, outcomes: &Outcomes) -> Result<Probabilities, Failure> {
        match self.noul {
            Some(yes) if is_probability(yes) => {
                Probabilities::read(outcomes, [("yes", yes), ("no", 1.0 - yes)])
            }
            _ => Err(Failure::Invalid),
        }
    }

    /// A `choice` answer's probabilities, keyed by outcome value.
    fn choice(&self, outcomes: &Outcomes) -> Result<Probabilities, Failure> {
        // `choice` has to name an outcome, not the likeliest one: the answer
        // settles from the probabilities alone, and two that the model tells
        // apart can tie once they are rounded to thousandths.
        let named = self
            .choice
            .as_deref()
            .is_none_or(|choice| outcomes.position(choice).is_some());

        if !named {
            return Err(Failure::Invalid);
        }

        let probabilities = self.probabilities.as_ref().ok_or(Failure::Invalid)?;

        Probabilities::read(
            outcomes,
            probabilities.iter().map(|(value, p)| (value.as_str(), *p)),
        )
    }

    /// A `score` answer's probabilities, keyed by level: `"0"` for the first
    /// outcome to n − 1 for the last. The level itself is not read from
    /// `score`, which one provider gives as the weighted level and another as
    /// the likeliest level's index, but both it and `expected_level` have to
    /// lie on the scale.
    fn score(&self, outcomes: &Outcomes) -> Result<Probabilities, Failure> {
        let top = top(outcomes);
        let on_scale = |level: Option<f64>| level.is_none_or(|level| (0.0..=top).contains(&level));

        if !on_scale(self.score) || !on_scale(self.expected_level) {
            return Err(Failure::Invalid);
        }

        // Every outcome's level is looked up by its exact key, and the count
        // matches, so no other key is left.
        let levels = self
            .probabilities
            .as_ref()
            .filter(|levels| levels.len() == outcomes.len())
            .ok_or(Failure::Invalid)?;
        let read: Vec<(&str, f64)> = outcomes
            .iter()
            .enumerate()
            .map(|(level, label)| Some((label.value(), *levels.get(&level.to_string())?)))
            .collect::<Option<_>>()
            .ok_or(Failure::Invalid)?;

        Probabilities::read(outcomes, read)
    }
}

/// Whether `outcomes` are exactly `yes` and `no`, which the API asks as
/// `noul`.
fn is_noul(outcomes: &Outcomes) -> bool {
    outcomes.len() == 2 && outcomes.position("yes").is_some() && outcomes.position("no").is_some()
}

fn is_probability(p: f64) -> bool {
    (0.0..=1.0).contains(&p)
}

/// The highest level of a scale whose levels are `outcomes`: n − 1.
fn top(outcomes: &Outcomes) -> f64 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "an outcome count is from 2 to 64, so the cast is exact"
    )]
    let levels = outcomes.len() as f64;
    levels - 1.0
}

/// The evidence the round's questions name, each block once.
pub(super) fn state(questions: &[&Question], evidence: &Evidence) -> String {
    let mut state = String::new();
    for (name, content) in evidence.named_by(questions) {
        let _ = write!(state, "{name}:\n{content}\n\n");
    }
    state
}
