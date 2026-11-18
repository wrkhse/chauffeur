//! What a navigation reports as it runs, and the verdict it ends in, as they
//! go on the wire.

use chauffeur_core::{Event as RunEvent, Failure, Outcome, Verdict};
use chauffeur_judges::Answer;
use serde_json::{Map, Value, json};

/// `rule_matched`: a rule's run started, its `on` and `when` having
/// matched the request.
#[must_use]
pub fn rule_matched(rule: &str) -> Value {
    json!({ "event": "rule_matched", "rule": rule })
}

/// A run's `round_ready` and `step_settled` events, in order, each naming
/// `rule`, or no rule for an inline strategy.
#[must_use]
pub fn run_events(rule: Option<&str>, events: &[RunEvent<Answer>]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| {
            let mut fields = match event {
                RunEvent::RoundReady { round, steps } => {
                    json!({ "event": "round_ready", "round": round, "steps": steps })
                }
                RunEvent::StepSettled { step, outcome } => {
                    let mut settled = settled(outcome);
                    if let Some(fields) = settled.as_object_mut() {
                        fields.insert("event".into(), json!("step_settled"));
                        fields.insert("step".into(), json!(step));
                    }
                    settled
                }
                // TODO(slice 3, PR 2): report a skipped step as `step_skipped` once
                // rules lower `after`; no rule's plan skips a step yet.
                RunEvent::Started { .. } | RunEvent::Finished | RunEvent::StepSkipped { .. } => {
                    return None;
                }
            };
            if let (Some(rule), Some(object)) = (rule, fields.as_object_mut()) {
                object.insert("rule".into(), json!(rule));
            }
            Some(fields)
        })
        .collect()
}

/// A step's result: `{ "answer": { … } }` or `{ "failed": "<reason>" }`.
/// Probabilities and the level have at most three decimals.
#[must_use]
pub fn settled(outcome: &Outcome<Answer>) -> Value {
    match outcome {
        Outcome::Value(answer) => {
            // A level that is not a number is reported as an invalid answer
            // rather than a null.
            if !answer.level().is_finite() {
                return json!({ "failed": Failure::Invalid.name() });
            }

            let p: Map<String, Value> = answer
                .p()
                .map(|(value, p)| (value.to_owned(), json!(p.to_f64())))
                .collect();
            let level = (answer.level() * 1000.0).round() / 1000.0;
            json!({ "answer": {
                "outcome": answer.outcome().to_string(),
                "p": p,
                "level": level,
                "model": answer.by().model,
                "placeholder": answer.by().placeholder,
            } })
        }
        Outcome::Failed(failure) => json!({ "failed": failure.name() }),
    }
}

/// A step's `step_settled` when its rule's run ended without settling: the
/// failure word names how, such as `deadline`, so a rule that started does
/// not leave its steps open.
#[must_use]
pub fn settled_failed(rule: &str, step: &str, failed: &str) -> Value {
    json!({ "event": "step_settled", "rule": rule, "step": step, "failed": failed })
}

/// Every step's result, by step ID.
#[must_use]
pub fn verdict(verdict: &Verdict<Answer>) -> Value {
    let steps: Map<String, Value> = verdict
        .iter()
        .map(|(step, outcome)| (step.to_owned(), settled(outcome)))
        .collect();
    Value::Object(steps)
}
