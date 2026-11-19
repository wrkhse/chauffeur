//! A step's `after` and `after_any` as core needs: a string need waits
//! for its step to finish, an object need for an answer near the bars of the
//! outcomes the rule acts on, or for one of its `when` values.

use std::collections::{BTreeMap, BTreeSet};

use chauffeur_core::{Bar, DECISION, Near, Need, When};
use chauffeur_judges::{Confidence, Label, Milli, Outcomes, Question};

use super::error::RuleError;
use super::file::{NeedFile, OutcomeNeedFile, StepFile, ThenFile};
use super::judge::JudgeSpec;
use crate::limits::{Exceeded, Limit, count};
use crate::then::Expect;

/// The real outcomes a rule's `then` acts on, by the step its `if` names,
/// read before the plan is built so a `near` need can find its bars.
#[derive(Default)]
pub(super) struct ActedOn {
    by_step: BTreeMap<String, BTreeSet<String>>,
}

impl ActedOn {
    /// The outcomes each `if` in `then` names, leaving out `unsure` and
    /// `failed`.
    pub(super) fn read(then: &[ThenFile]) -> Self {
        let mut by_step: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for (step, wanted) in then.iter().flat_map(|entry| &entry.needs) {
            if let Expect::Outcome(outcome) = Expect::parse(wanted) {
                by_step.entry(step.clone()).or_default().insert(outcome);
            }
        }

        Self { by_step }
    }

    /// The outcomes `then` acts on for the `if` key `step`.
    fn labels(&self, step: &str) -> impl Iterator<Item = &str> {
        self.by_step
            .get(step)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
}

/// Checks the sizes of a step's needs before anything reads them: each list,
/// each `when`, and the bytes of every step ID and outcome they name.
pub(super) fn check_bounds(step: &StepFile) -> Result<(), Exceeded> {
    for needs in [&step.after, &step.after_any] {
        Limit::Needs.check(count(needs.len()))?;

        for need in needs {
            match need {
                NeedFile::Settled(target) => Limit::TextBytes.check(count(target.len()))?,
                NeedFile::Outcome(need) => check_object(need)?,
            }
        }
    }

    Ok(())
}

fn check_object(need: &OutcomeNeedFile) -> Result<(), Exceeded> {
    Limit::TextBytes.check(count(need.step.len()))?;

    if let Some(outcome) = &need.outcome {
        Limit::TextBytes.check(count(outcome.len()))?;
    }

    if let Some(when) = &need.when {
        Limit::WhenOutcomes.check(count(when.len()))?;

        for value in when {
            Limit::TextBytes.check(count(value.len()))?;
        }
    }

    Ok(())
}

/// A step with its question and judge resolved, its needs as written.
pub(super) struct Resolved {
    pub(super) id: String,
    pub(super) judge: String,
    pub(super) question: Question,
    pub(super) after: Vec<NeedFile>,
    pub(super) after_any: Vec<NeedFile>,
}

/// What lowering a need reads: the resolved steps, their judges, and what
/// the rule's `then` acts on.
pub(super) struct Lowering<'a> {
    pub(super) rule: &'a str,
    pub(super) steps: &'a [Resolved],
    pub(super) judges: &'a BTreeMap<String, JudgeSpec>,
    pub(super) acted_on: &'a ActedOn,
}

impl Lowering<'_> {
    /// The `after` and `after_any` of `step`, at `index` in the plan, as
    /// core needs in the order written. Which steps they name, and whether
    /// those come earlier, is left to `Plan::new`.
    pub(super) fn needs(
        &self,
        index: usize,
        step: &Resolved,
    ) -> Result<(Vec<Need>, Vec<Need>), RuleError> {
        let earlier = self.steps.get(..index).unwrap_or_default();
        let lower = |needs: &[NeedFile]| -> Result<Vec<Need>, RuleError> {
            needs
                .iter()
                .map(|need| self.need(&step.id, earlier, need))
                .collect()
        };

        Ok((lower(&step.after)?, lower(&step.after_any)?))
    }

    /// One need of `consumer`, whose target is read only among the steps
    /// `earlier` than it.
    fn need(
        &self,
        consumer: &str,
        earlier: &[Resolved],
        need: &NeedFile,
    ) -> Result<Need, RuleError> {
        let need = match need {
            NeedFile::Settled(target) => return Ok(Need::settled(target.as_str())),
            NeedFile::Outcome(need) => need,
        };
        let refuse = |reason: String| RuleError::Need {
            rule: self.rule.to_owned(),
            step: consumer.to_owned(),
            need: need.step.clone(),
            reason,
        };

        let margin = match (need.near, &need.outcome) {
            (None, Some(_)) => return Err(refuse("`outcome` comes only with `near`".to_owned())),
            (None, None) => None,
            (Some(near), _) => Some(
                Milli::from_f64(near)
                    .ok_or_else(|| refuse(format!("`near` {near} is not a margin from 0 to 1")))?,
            ),
        };

        // An unknown, later or the same step is `Plan::new`'s to refuse, as
        // `UnknownNeed` or `LaterNeed`, before its bars or `when` are read.
        let Some(target) = earlier.iter().find(|step| step.id == need.step) else {
            return Ok(Need::outcome(need.step.as_str(), None, None));
        };

        let when = need
            .when
            .as_deref()
            .map(|when| values(target.question.outcomes(), when))
            .transpose()
            .map_err(refuse)?;
        let near = margin
            .map(|margin| self.near(target, margin, need.outcome.as_deref()))
            .transpose()
            .map_err(refuse)?;

        Ok(Need::outcome(need.step.as_str(), near, when))
    }

    /// The bars `near` matches around: the target judge's bar on each
    /// outcome named by `outcome`, or else on each the rule acts on.
    fn near(
        &self,
        target: &Resolved,
        margin: Milli,
        outcome: Option<&str>,
    ) -> Result<Near, String> {
        let outcomes = target.question.outcomes();
        let selected = match outcome {
            Some(outcome) => {
                let index = outcomes.position(outcome).ok_or_else(|| {
                    format!("`outcome` names `{outcome}`, which is not one of its outcomes")
                })?;
                BTreeSet::from([index])
            }
            None => self.acted_on_by(target),
        };

        if selected.is_empty() {
            return Err("`near` has no outcome to bar: name one in `outcome`, or act on one in `then` or a later `when`".to_owned());
        }

        let bars = selected
            .into_iter()
            .map(|index| self.bar(target, index))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Near::new(margin.get(), bars))
    }

    /// The outcomes of `target` the rule acts on: those its `then` expects
    /// of it or of the decision, and those any need on it lists in `when`.
    fn acted_on_by(&self, target: &Resolved) -> BTreeSet<usize> {
        let outcomes = target.question.outcomes();
        let when = self
            .steps
            .iter()
            .flat_map(|step| step.after.iter().chain(&step.after_any))
            .filter_map(|need| match need {
                NeedFile::Outcome(need) if need.step == target.id => need.when.as_deref(),
                NeedFile::Outcome(_) | NeedFile::Settled(_) => None,
            })
            .flatten()
            .map(String::as_str);

        self.acted_on
            .labels(&target.id)
            .chain(self.acted_on.labels(DECISION))
            .chain(when)
            .filter_map(|label| outcomes.position(label))
            .collect()
    }

    /// The bar `target`'s judge sets on the outcome at `index`.
    fn bar(&self, target: &Resolved, index: usize) -> Result<Bar, String> {
        let label = target
            .question
            .outcomes()
            .get(index)
            .map_or_default(Label::value);
        let judge = &target.judge;
        let at = match self.judges.get(judge).map(|spec| &spec.confidence) {
            Some(Confidence::Bar(bar)) => Some(*bar),
            Some(Confidence::PerOutcome(bars)) => bars.get(label).copied(),
            Some(Confidence::Likeliest) | None => {
                return Err(format!(
                    "`near` needs a bar, and judge `{judge}` has none: it settles on the likeliest outcome"
                ));
            }
        };
        let at = at.ok_or_else(|| format!("`near` needs judge `{judge}`'s bar for `{label}`"))?;
        let outcome = index_of(index)?;

        Ok(Bar {
            outcome,
            at: at.get(),
        })
    }
}

/// A `when` list as values of a step with `outcomes`: an outcome by its
/// index, `unsure` after every outcome, and `failed`.
fn values(outcomes: &Outcomes, when: &[String]) -> Result<Vec<When>, String> {
    when.iter()
        .map(|value| match value.as_str() {
            "failed" => Ok(When::Failed),
            "unsure" => index_of(outcomes.len()).map(When::Outcome),
            label => outcomes
                .position(label)
                .ok_or_else(|| format!("`when` names `{label}`, which is not one of its outcomes"))
                .and_then(index_of)
                .map(When::Outcome),
        })
        .collect()
}

/// An outcome's index as core counts it; a question has at most 64.
fn index_of(index: usize) -> Result<u8, String> {
    u8::try_from(index).map_err(|_| format!("outcome {index} is past what a step holds"))
}
