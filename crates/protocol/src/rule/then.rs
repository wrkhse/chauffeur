use chauffeur_core::{DECISION, Plan};
use chauffeur_judges::Question;

use super::append::check_append;
use super::error::RuleError;
use super::file::ThenFile;
use crate::then::{Expect, Then};

/// The `then` entry at `index`, its `if` checked against the plan's steps
/// and their outcomes, with exactly one action or intent. `decision`, the
/// last step that opened, expects an outcome some step has.
pub(super) fn resolve_then(
    rule: &str,
    index: usize,
    entry: ThenFile,
    plan: &Plan<Question>,
) -> Result<Then, RuleError> {
    let refuse = |reason: String| RuleError::Then {
        rule: rule.to_owned(),
        index,
        reason,
    };
    let mut needs = Vec::with_capacity(entry.needs.len());
    for (step_id, wanted) in entry.needs {
        let expect = Expect::parse(&wanted);
        // The plan refuses a step named `decision`, so it is always the
        // virtual one.
        let step = (step_id != DECISION)
            .then(|| plan.steps().iter().find(|step| step.id() == step_id))
            .flatten();

        match (step, &expect) {
            (Some(step), Expect::Outcome(outcome))
                if step.request().outcomes().position(outcome).is_none() =>
            {
                return Err(refuse(format!(
                    "expects `{outcome}` from step `{step_id}`, which is not one of its outcomes"
                )));
            }
            (None, Expect::Outcome(outcome)) if step_id == DECISION && !any_has(plan, outcome) => {
                return Err(refuse(format!(
                    "expects `{outcome}` from `{DECISION}`, which no step has as an outcome"
                )));
            }
            (None, _) if step_id != DECISION => {
                return Err(refuse(format!(
                    "names step `{step_id}`, which the strategy lacks"
                )));
            }
            _ => {}
        }

        needs.push((step_id, expect));
    }
    let mut does = entry.does.into_iter();
    match (does.next(), does.next()) {
        // Two entries are refused below whatever their names.
        (Some((name, _)), None) if name == "rule" => Err(refuse(
            "cannot name an action or intent `rule`, which names the rule on the wire".to_owned(),
        )),
        (Some((name, payload)), None) => {
            if name == "message.append" {
                check_append(&payload).map_err(refuse)?;
            }
            Ok(Then::new(needs, name, payload))
        }
        _ => Err(refuse(
            "needs exactly one action or intent beside `if`".to_owned(),
        )),
    }
}

/// Whether any of the plan's questions has `outcome`.
fn any_has(plan: &Plan<Question>, outcome: &str) -> bool {
    plan.steps()
        .iter()
        .any(|step| step.request().outcomes().position(outcome).is_some())
}
