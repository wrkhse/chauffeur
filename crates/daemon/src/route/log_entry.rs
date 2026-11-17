use chauffeur_core::{Outcome, Run};
use chauffeur_judges::Answer;
use chauffeur_runtime::Ran;
use serde_json::{Map, Value, json};

/// Writes how a run ended into its log `entry`, and gives back its run if
/// it settled.
pub(super) fn ended(entry: &mut Map<String, Value>, ran: Ran) -> Option<Run<Answer>> {
    match ran {
        Ran::Settled(run) => {
            entry.insert(
                "steps".into(),
                steps(&run.verdict.iter().collect::<Vec<_>>()),
            );
            Some(run)
        }
        Ran::Refused(reason) => {
            entry.insert("refused".into(), json!(reason));
            None
        }
        Ran::Busy => {
            entry.insert("refused".into(), json!("the pool could not start the run"));
            None
        }
        Ran::Stopped => {
            entry.insert("refused".into(), json!("the run's worker stopped"));
            None
        }
        Ran::Deadline => {
            entry.insert("deadline".into(), json!(true));
            None
        }
        Ran::Cancelled => {
            entry.insert("cancelled".into(), json!(true));
            None
        }
        // A variant a newer runtime adds: it has no result, so it reads as
        // refused.
        _ => {
            entry.insert("refused".into(), json!("the run ended without a result"));
            None
        }
    }
}

/// The word for a run that ended without settling, as a `step_settled`
/// names it: `deadline`, `cancelled`, `busy`, `stopped` or `refused`. None
/// when the run settled.
pub(super) const fn unsettled(ran: &Ran) -> Option<&'static str> {
    match ran {
        Ran::Settled(_) => None,
        Ran::Deadline => Some("deadline"),
        Ran::Cancelled => Some("cancelled"),
        Ran::Busy => Some("busy"),
        Ran::Stopped => Some("stopped"),
        // A refusal, and a variant a newer runtime adds, has no result.
        _ => Some("refused"),
    }
}

/// Each step's answer or failure, for the log.
pub(super) fn steps(verdict: &[(&str, &Outcome<Answer>)]) -> Value {
    let steps = verdict
        .iter()
        .map(|(step, outcome)| match outcome {
            Outcome::Value(answer) => json!({ "step": step, "answer": answer_json(answer) }),
            Outcome::Failed(failure) => json!({ "step": step, "failed": failure.name() }),
        })
        .collect();
    Value::Array(steps)
}

fn answer_json(answer: &Answer) -> Value {
    let p: Map<String, Value> = answer
        .p()
        .map(|(value, p)| (value.to_owned(), json!(p.to_f64())))
        .collect();
    json!({
        "outcome": answer.outcome().to_string(),
        "p": p,
        "level": answer.level(),
        "model": answer.by().model,
        "placeholder": answer.by().placeholder,
    })
}
