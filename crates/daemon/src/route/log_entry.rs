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

#[cfg(test)]
mod tests {
    use chauffeur_runtime::Ran;
    use serde_json::{Map, Value, json};

    use super::{ended, unsettled};

    /// Each way a run can end without settling writes its own word into its
    /// log entry, and gives no run back: a refusal's reason, the pool's own
    /// words for a busy pool and a stopped worker, and a flag for the
    /// deadline and the caller's cancellation. A settled run writes its steps
    /// and gives its run back, which needs a verdict only core builds; the
    /// socket's nudge test covers that line.
    #[test]
    fn each_unsettled_end_writes_its_own_word_and_gives_no_run_back() {
        let cases: [(Ran, Value, Option<&'static str>); 5] = [
            (
                Ran::Refused("no system-one model is registered".to_owned()),
                json!({ "refused": "no system-one model is registered" }),
                Some("refused"),
            ),
            (
                Ran::Busy,
                json!({ "refused": "the pool could not start the run" }),
                Some("busy"),
            ),
            (
                Ran::Stopped,
                json!({ "refused": "the run's worker stopped" }),
                Some("stopped"),
            ),
            (Ran::Deadline, json!({ "deadline": true }), Some("deadline")),
            (
                Ran::Cancelled,
                json!({ "cancelled": true }),
                Some("cancelled"),
            ),
        ];

        for (ran, logged, word) in cases {
            assert_eq!(unsettled(&ran), word, "the word for {logged}");

            let mut entry = Map::new();
            let run = ended(&mut entry, ran);

            assert!(run.is_none(), "no run comes back for {logged}");
            assert_eq!(Value::Object(entry), logged);
        }
    }
}
