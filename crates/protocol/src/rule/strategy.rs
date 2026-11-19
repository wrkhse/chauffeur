use std::collections::BTreeMap;

use chauffeur_core::{DECISION, Plan, PlanError, Step};
use chauffeur_judges::{ConfidenceError, Question, Tier};

use super::ask;
use super::error::RuleError;
use super::file::{AskFile, AskRef, JudgeFile, StepFile};
use super::judge::{self, JudgeSpec};
use super::need::{self, ActedOn, Lowering, Resolved};
use crate::limits::{Exceeded, Limit, count};

/// A strategy: the judges its steps name, and its steps as a core plan.
/// A rule holds one, and a request may send one inline.
#[derive(Debug)]
pub struct Strategy {
    judges: BTreeMap<String, JudgeSpec>,
    plan: Plan<Question>,
}

impl AsRef<Self> for Strategy {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl Strategy {
    /// Checks the inline strategy of `id`, which has no `then`: a `near`
    /// need bars the outcome it names, or those a later need's `when` lists.
    pub(crate) fn build(
        id: &str,
        judges: BTreeMap<String, JudgeFile>,
        asks: &BTreeMap<String, AskFile>,
        strategy: Vec<StepFile>,
    ) -> Result<Self, RuleError> {
        Self::lower(id, judges, asks, strategy, &ActedOn::default())
    }

    /// Checks the strategy of `id`: each step's judge and question, each
    /// judge's confidence against the outcomes of the steps it answers, and
    /// each need, whose `near` bars the outcomes `acted_on` names.
    pub(super) fn lower(
        id: &str,
        judges: BTreeMap<String, JudgeFile>,
        asks: &BTreeMap<String, AskFile>,
        strategy: Vec<StepFile>,
        acted_on: &ActedOn,
    ) -> Result<Self, RuleError> {
        check_bounds(id, &judges, asks, &strategy)?;

        let mut specs = BTreeMap::new();
        for (name, judge) in judges {
            specs.insert(name.clone(), judge::judge_spec(id, &name, judge)?);
        }
        let resolved = resolve_steps(id, strategy, asks, &mut specs)?;

        let lowering = Lowering {
            rule: id,
            steps: &resolved,
            judges: &specs,
            acted_on,
        };
        let needs = resolved
            .iter()
            .enumerate()
            .map(|(index, step)| lowering.needs(index, step))
            .collect::<Result<Vec<_>, _>>()?;
        let steps = resolved.into_iter().zip(needs).map(|(step, (all, any))| {
            Step::new(step.id, step.judge, step.question)
                .after(all)
                .after_any(any)
        });

        let plan = Plan::new(steps).map_err(|source| RuleError::Plan {
            rule: id.to_owned(),
            source,
        })?;
        Ok(Self {
            judges: specs,
            plan,
        })
    }

    /// The judges its steps name, including tiers named directly.
    #[must_use]
    pub const fn judges(&self) -> &BTreeMap<String, JudgeSpec> {
        &self.judges
    }

    /// Whether any judge its steps name, including a tier named directly, is
    /// of `tier`.
    #[must_use]
    pub fn uses_tier(&self, tier: Tier) -> bool {
        self.judges.values().any(|judge| judge.tier == tier)
    }

    /// Its steps as a core plan.
    #[must_use]
    pub const fn plan(&self) -> &Plan<Question> {
        &self.plan
    }
}

/// Checks the strategy's counts and the sizes of everything it names, then
/// that every step has an ID and none is the reserved `decision`, before
/// anything reads them.
fn check_bounds(
    id: &str,
    judges: &BTreeMap<String, JudgeFile>,
    asks: &BTreeMap<String, AskFile>,
    strategy: &[StepFile],
) -> Result<(), RuleError> {
    let limit = |source| RuleError::Limit {
        rule: id.to_owned(),
        source,
    };
    Limit::Steps.check(count(strategy.len())).map_err(limit)?;
    Limit::Judges.check(count(judges.len())).map_err(limit)?;
    Limit::Asks.check(count(asks.len())).map_err(limit)?;
    check_names(judges, asks, strategy).map_err(limit)?;

    // Every ask, used or not, so an unused one cannot carry unbounded text.
    let inline = strategy.iter().filter_map(|step| match &step.ask {
        AskRef::Inline(ask) => Some(ask),
        AskRef::Named(_) => None,
    });
    for ask in asks.values().chain(inline) {
        ask::check_bounds(ask).map_err(limit)?;
    }

    if strategy.iter().any(|step| step.id.trim().is_empty()) {
        return Err(RuleError::BlankStep {
            rule: id.to_owned(),
        });
    }

    // `Plan::new` refuses it too; refusing it here keeps a need or `then`
    // on it from being read as the step before the plan is built.
    if strategy.iter().any(|step| step.id == DECISION) {
        return Err(RuleError::Plan {
            rule: id.to_owned(),
            source: PlanError::ReservedStep(DECISION.to_owned()),
        });
    }

    Ok(())
}

/// Each step's question and judge, its confidence checked against the
/// question's outcomes, with its needs still as written.
fn resolve_steps(
    id: &str,
    strategy: Vec<StepFile>,
    asks: &BTreeMap<String, AskFile>,
    specs: &mut BTreeMap<String, JudgeSpec>,
) -> Result<Vec<Resolved>, RuleError> {
    let mut resolved = Vec::with_capacity(strategy.len());

    for step in strategy {
        let question = ask::resolve_ask(id, &step, asks)?;
        let spec = judge::resolve_judge(id, &step, specs)?;
        spec.confidence
            .check(question.outcomes())
            .map_err(|error: ConfidenceError| RuleError::Confidence {
                rule: id.to_owned(),
                step: step.id.clone(),
                reason: error.to_string(),
            })?;

        resolved.push(Resolved {
            id: step.id,
            judge: step.judge,
            question,
            after: step.after,
            after_any: step.after_any,
        });
    }

    Ok(resolved)
}

/// Checks the bytes of every name `judges`, `asks` and the steps use, and
/// the sizes of the steps' needs, before anything reads them.
fn check_names(
    judges: &BTreeMap<String, JudgeFile>,
    asks: &BTreeMap<String, AskFile>,
    strategy: &[StepFile],
) -> Result<(), Exceeded> {
    for (name, judge) in judges {
        Limit::TextBytes.check(count(name.len()))?;

        if let Some(model) = &judge.model {
            Limit::TextBytes.check(count(model.len()))?;
        }
    }

    for name in asks.keys() {
        Limit::TextBytes.check(count(name.len()))?;
    }

    for step in strategy {
        Limit::TextBytes.check(count(step.id.len()))?;
        Limit::TextBytes.check(count(step.judge.len()))?;
        if let AskRef::Named(name) = &step.ask {
            Limit::TextBytes.check(count(name.len()))?;
        }
        need::check_bounds(step)?;
    }

    Ok(())
}
