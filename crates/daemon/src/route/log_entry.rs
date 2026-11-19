use chauffeur_core::{Event, Outcome, Run};
use chauffeur_judges::Answer;
use chauffeur_runtime::Ran;
use serde_json::{Map, Value, json};

/// Writes how a run ended into its log `entry`, and gives back its run if
/// it settled.
pub(super) fn ended(entry: &mut Map<String, Value>, ran: Ran) -> Option<Run<Answer>> {
    match ran {
        Ran::Settled(run) => {
            entry.insert("steps".into(), steps(&run));
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

/// Each step of a settled run, in plan order, for the log: its answer, its
/// failure, or `"skipped": true` for a step its strategy did not need. The
/// plan order is the run's `Started` event; a step neither settled nor
/// skipped is left out.
fn steps(run: &Run<Answer>) -> Value {
    let order = run
        .events
        .iter()
        .find_map(|event| match event {
            Event::Started { steps } => Some(steps.as_slice()),
            _ => None,
        })
        .unwrap_or_default();
    let skipped: Vec<&str> = run.verdict.skipped().collect();

    let steps = order
        .iter()
        .filter_map(|step| match run.verdict.get(step) {
            Some(Outcome::Value(answer)) => {
                Some(json!({ "step": step, "answer": answer_json(answer) }))
            }
            Some(Outcome::Failed(failure)) => {
                Some(json!({ "step": step, "failed": failure.name() }))
            }
            None => skipped
                .contains(&step.as_str())
                .then(|| json!({ "step": step, "skipped": true })),
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
    use chauffeur_core::{Failure, Need, Outcome, Panel, Plan, Step, When};
    use chauffeur_judges::{Answer, AnsweredBy, Confidence, Probabilities, Question};
    use chauffeur_runtime::Ran;
    use serde_json::{Map, Value, json};

    use super::{ended, unsettled};

    /// Each way a run can end without settling writes its own word into its
    /// log entry, and gives no run back: a refusal's reason, the pool's own
    /// words for a busy pool and a stopped worker, and a flag for the
    /// deadline and the caller's cancellation.
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

    /// A settled run gives its run back and logs every step in plan order:
    /// an answer, a failure, and `"skipped": true` for `sure`, which runs
    /// only when `fast` fails. The step's flag is a bool, apart from a
    /// run's `"skipped": "deadline"`.
    #[test]
    fn a_settled_run_logs_each_step_in_plan_order_with_its_skipped_steps() {
        let question = Question::noul("Is this message conventional?", ["diff"]).unwrap();
        let failed = Need::outcome("fast", None, Some(vec![When::Failed]));
        let plan = Plan::new([
            Step::new("fast", "jev", question.clone()),
            Step::new("sure", "glide", question.clone()).after([failed]),
            Step::new("last", "tail", question).after([Need::settled("fast")]),
        ])
        .unwrap();

        let yes = |question: &Question| {
            let p =
                Probabilities::read(question.outcomes(), [("yes", 0.94), ("no", 0.06)]).unwrap();
            let by = AnsweredBy {
                model: "jev".into(),
                placeholder: false,
            };

            Outcome::Value(Answer::settle(question, p, &Confidence::Likeliest, by))
        };
        let timeout = |_: &Question| Outcome::Failed(Failure::Timeout);
        let panel = Panel::new()
            .with("jev", &yes)
            .with("glide", &yes)
            .with("tail", &timeout);
        let run = plan.run(&panel).unwrap();

        let mut entry = Map::new();
        let returned = ended(&mut entry, Ran::Settled(run.clone()));

        assert_eq!(returned, Some(run));
        assert_eq!(
            Value::Object(entry),
            json!({
                "steps": [
                    {
                        "step": "fast",
                        "answer": {
                            "outcome": "yes",
                            "p": { "yes": 0.94, "no": 0.06 },
                            "level": 0.06,
                            "model": "jev",
                            "placeholder": false,
                        },
                    },
                    { "step": "sure", "skipped": true },
                    { "step": "last", "failed": "timeout" },
                ],
            })
        );
    }
}
