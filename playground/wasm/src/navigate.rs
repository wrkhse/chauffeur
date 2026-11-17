use std::collections::{BTreeMap, BTreeSet};

use chauffeur_core::{Failure, Outcome, Panel, Plan, Verdict};
use chauffeur_judges::{Answer, AnsweredBy, Probabilities, Question};
use chauffeur_protocol::{Intent, JudgeSpec, Navigate, Rule, Target, Then, progress};
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::prelude::wasm_bindgen;

/// What a stand-in judge answers for one step: a probability per outcome,
/// or a failure.
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum RawReply {
    Failed { failed: String },
    P(BTreeMap<String, f64>),
}

enum Reply {
    Failed(Failure),
    P(BTreeMap<String, f64>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request: Value,
    /// Rule files by ID, as the daemon would load them.
    #[serde(default)]
    rules: BTreeMap<String, Value>,
    /// Each step's stand-in reply, by step ID.
    #[serde(default)]
    answers: BTreeMap<String, Value>,
}

/// Navigates as the daemon does, with stand-in replies in place of models.
///
/// Takes `{ request, rules, answers }` as JSON: a `chauffeur.navigate`
/// request's params, rule files by ID, and each step's reply by step ID.
/// Returns `{ events, result, refused }`, where `result` is `{ intents }`
/// or `{ verdict }`; `{ rejected }`, the JSON-RPC error a caller gets; or
/// `{ error }` for input the playground cannot read. A rule that does not
/// load, or whose plan cannot run, is listed in `refused` and the others
/// still run; a reply keyed to a step no plan has is an error.
#[wasm_bindgen]
#[must_use]
pub fn navigate(input: &str) -> String {
    navigate_json(input)
        .unwrap_or_else(|error| json!({ "error": error }))
        .to_string()
}

fn navigate_json(input: &str) -> Result<Value, String> {
    let input: Input = serde_json::from_str(input).map_err(|error| error.to_string())?;
    let request = match Navigate::parse(&input.request) {
        Ok(request) => request,
        Err(rejection) => return Ok(json!({ "rejected": rejection.to_error() })),
    };
    // The daemon never reads an answer for a request it rejects, so a
    // rejection is the caller's error even when the answers are malformed.
    let answers = parse_answers(input.answers)?;
    let mut events = Vec::new();
    let mut refused = Vec::new();
    let mut steps = BTreeSet::new();
    let result = match (request.target(), request.event()) {
        (Target::Strategy(strategy), _) => {
            steps.extend(step_ids(strategy.plan()));
            let verdict = run(
                strategy.plan(),
                strategy.judges(),
                &answers,
                None,
                &mut events,
            )?;
            json!({ "verdict": progress::verdict(&verdict) })
        }
        (Target::On(_), Some(event)) => {
            let mut matched = Vec::new();
            for (id, text) in &input.rules {
                let rule = match Rule::parse(id, &text.to_string()) {
                    Ok(rule) => rule,
                    Err(error) => {
                        refused.push(error.to_string());
                        continue;
                    }
                };
                steps.extend(step_ids(rule.strategy().plan()));
                if !rule.hooks(&event) {
                    continue;
                }
                events.push(progress::rule_matched(id));
                matched.push((id, rule));
            }
            let hooked: Vec<Hooked> = matched
                .iter()
                .map(|(id, rule)| Hooked::of(id.as_str(), rule))
                .collect();
            let intents = run_hooked(&hooked, &answers, &mut events, &mut refused);
            json!({ "intents": intents })
        }
        (Target::On(_), None) => return Err("an event request has no event".to_owned()),
    };
    check_answers(&answers, &steps)?;
    Ok(json!({ "events": events, "result": result, "refused": refused }))
}

fn parse_answers(values: BTreeMap<String, Value>) -> Result<BTreeMap<String, Reply>, String> {
    values.into_iter().map(|(step, value)| {
        let reply: RawReply = serde_json::from_value(value)
            .map_err(|_| format!("answers.{step}: expected {{outcome:p,...}} or {{failed:name}}"))?;
        let reply = match reply {
            RawReply::Failed { failed } => Reply::Failed(Failure::from_name(&failed)
                .ok_or_else(|| format!("answers.{step}: unknown failure {failed}; expected unavailable, rate_limited, timeout, invalid, deadline"))?),
            RawReply::P(p) => Reply::P(p),
        };
        Ok((step, reply))
    }).collect()
}

/// A rule that hooks the event, broken into what running it needs. A rule
/// the protocol accepts always has a panel for its plan, so keeping the plan
/// and its judges apart is also what lets a test hand the loop a plan that
/// cannot run.
struct Hooked<'a> {
    id: &'a str,
    plan: &'a Plan<Question>,
    judges: &'a BTreeMap<String, JudgeSpec>,
    then: &'a [Then],
}

impl<'a> Hooked<'a> {
    fn of(id: &'a str, rule: &'a Rule) -> Self {
        Self {
            id,
            plan: rule.plan(),
            judges: rule.judges(),
            then: rule.then(),
        }
    }
}

/// Runs each hooked rule and returns the intents of the ones that ran. One
/// that cannot run is recorded in `refused`, as the daemon records it, and
/// the others still run.
fn run_hooked(
    hooked: &[Hooked<'_>],
    answers: &BTreeMap<String, Reply>,
    events: &mut Vec<Value>,
    refused: &mut Vec<String>,
) -> Vec<Value> {
    let mut intents = Vec::new();
    for rule in hooked {
        match run(rule.plan, rule.judges, answers, Some(rule.id), events) {
            Ok(verdict) => {
                let met = Then::first_met(rule.then, &verdict);
                intents.extend(met.map(|then| Intent::new(rule.id, then).to_json()));
            }
            Err(error) => {
                let id = rule.id;
                refused.push(format!("rule `{id}`: {error}"));
            }
        }
    }
    intents
}

/// The IDs of a plan's steps, so a reply keyed to none of them is refused.
fn step_ids(plan: &Plan<Question>) -> impl Iterator<Item = String> + '_ {
    plan.steps().iter().map(|step| step.id().to_owned())
}

/// Refuses a reply keyed to a step no plan has, which would be ignored.
fn check_answers(
    answers: &BTreeMap<String, Reply>,
    steps: &BTreeSet<String>,
) -> Result<(), String> {
    answers
        .keys()
        .find(|step| !steps.contains(*step))
        .map_or(Ok(()), |step| Err(format!("answers.{step}: no such step")))
}

/// Runs `plan` with each step answered by its stand-in reply, settled under
/// its judge's confidence, and records the run's events.
fn run(
    plan: &Plan<Question>,
    specs: &BTreeMap<String, JudgeSpec>,
    answers: &BTreeMap<String, Reply>,
    rule: Option<&str>,
    events: &mut Vec<Value>,
) -> Result<Verdict<Answer>, String> {
    // Plan::run passes the stored step request by reference to its judge.
    // Its identity distinguishes steps even when their questions are equal.
    let step_of = |question: &Question| {
        plan.steps()
            .iter()
            .find(|step| std::ptr::eq(step.request(), question))
            .map(|step| step.id().to_owned())
    };
    let judges: Vec<(&String, _)> = specs
        .iter()
        .map(|(name, spec)| {
            let judge = move |question: &Question| {
                let reply = step_of(question).and_then(|step| answers.get(&step));
                stand_in(question, reply, &spec.confidence)
            };
            (name, judge)
        })
        .collect();
    let panel = judges.iter().fold(Panel::new(), |panel, (name, judge)| {
        panel.with(name.as_str(), judge)
    });
    let ran = plan.run(&panel).map_err(|error| error.to_string())?;
    events.extend(progress::run_events(rule, &ran.events));
    Ok(ran.verdict)
}

/// A step's stand-in answer: its reply settled under `confidence`, or the
/// failure it names; a step with no reply is `unavailable`, as with no model.
fn stand_in(
    question: &Question,
    reply: Option<&Reply>,
    confidence: &chauffeur_judges::Confidence,
) -> Outcome<Answer> {
    let by = AnsweredBy {
        model: "stand-in".into(),
        placeholder: true,
    };
    match reply {
        None => Outcome::Failed(Failure::Unavailable),
        Some(Reply::Failed(failure)) => Outcome::Failed(*failure),
        Some(Reply::P(p)) => {
            match Probabilities::read(
                question.outcomes(),
                p.iter().map(|(value, p)| (value.as_str(), *p)),
            ) {
                Ok(p) => Outcome::Value(Answer::settle(question, p, confidence, by)),
                Err(failure) => Outcome::Failed(failure),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chauffeur_core::{Plan, Step};
    use chauffeur_judges::Question;
    use chauffeur_protocol::Rule;
    use serde_json::json;

    use super::{Hooked, Reply, run_hooked};

    /// A yes or no reply, likely enough to settle under the rule's confidence.
    fn yes() -> Reply {
        Reply::P(BTreeMap::from([
            ("yes".to_owned(), 0.9),
            ("no".to_owned(), 0.1),
        ]))
    }

    /// A rule the protocol accepts always has a judge for each of its steps,
    /// so a plan that names one it lacks cannot come from a request; its rule
    /// is refused while the next rule still returns its intent.
    #[test]
    fn a_rule_that_cannot_run_is_refused_and_the_others_still_run() {
        let question = Question::noul("Show it?", ["event"]).unwrap();
        let broken = Plan::new([Step::new("screen", "missing", question)]).unwrap();
        let judges = BTreeMap::new();
        let good = Rule::parse(
            "b",
            &json!({
                "on": "sourcefed.event",
                "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
                "strategy": [{ "id": "screen", "judge": "quick",
                               "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                                        "instructions": "Show it?" } }],
                "then": [{ "if": { "screen": "yes" }, "withhold": "event" }]
            })
            .to_string(),
        )
        .unwrap();
        let answers = BTreeMap::from([("screen".to_owned(), yes())]);
        let mut events = Vec::new();
        let mut refused = Vec::new();

        let intents = run_hooked(
            &[
                Hooked {
                    id: "a",
                    plan: &broken,
                    judges: &judges,
                    then: &[],
                },
                Hooked::of("b", &good),
            ],
            &answers,
            &mut events,
            &mut refused,
        );

        assert_eq!(
            refused.first().map(String::as_str),
            Some("rule `a`: step `screen` names judge `missing`, which the panel lacks")
        );
        assert_eq!(intents, vec![json!({ "rule": "b", "withhold": "event" })]);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.first(),
            Some(
                &json!({ "event": "round_ready", "round": 1, "steps": ["screen"],
                          "rule": "b" })
            )
        );
    }
}
