//! A step's `after` and `after_any` as rules and inline strategies write them,
//! lowered to core needs and run with judges whose answers settle under the
//! rule's own confidence.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture"
)]

use chauffeur_core::{Bar, DECISION, Failure, Judge, Near, Need, Outcome, Panel, Run, When};
use chauffeur_judges::{Answer, AnsweredBy, Confidence, Probabilities, Question};
use chauffeur_protocol::{
    Exceeded, Limit, Navigate, Rejection, Rule, RuleError, Strategy, Target, Then, progress,
};
use serde_json::{Value, json};

/// A judge whose answer gives the question's first outcome `first`, and
/// the second the rest, settled under its rule's confidence; or a failure.
struct Says {
    confidence: Confidence,
    first: Result<f64, Failure>,
}

impl Judge<Question, Answer> for Says {
    fn judge(&self, question: &Question) -> Outcome<Answer> {
        let first = match self.first {
            Ok(first) => first,
            Err(failure) => return Outcome::Failed(failure),
        };
        let labels: Vec<&str> = question
            .outcomes()
            .iter()
            .map(chauffeur_judges::Label::value)
            .collect();
        let p = Probabilities::read(
            question.outcomes(),
            [(labels[0], first), (labels[1], 1.0 - first)],
        )
        .unwrap();
        let by = AnsweredBy {
            model: "stand-in".into(),
            placeholder: true,
        };

        Outcome::Value(Answer::settle(question, p, &self.confidence, by))
    }
}

/// Runs `strategy` with each named judge answering as given; a judge not
/// named is unavailable, so a step it should not answer shows as failed.
fn run(strategy: &Strategy, answers: &[(&str, Result<f64, Failure>)]) -> Run<Answer> {
    let says: Vec<(&str, Says)> = strategy
        .judges()
        .iter()
        .map(|(name, spec)| {
            let confidence = spec.confidence.clone();
            let first = answers
                .iter()
                .find(|(answering, _)| answering == name)
                .map_or(Err(Failure::Unavailable), |(_, first)| *first);

            (name.as_str(), Says { confidence, first })
        })
        .collect();
    let panel = says.iter().fold(Panel::new(), |panel, (name, judge)| {
        panel.with(*name, judge)
    });

    strategy.plan().run(&panel).unwrap()
}

/// A run's wire events and the steps it skipped.
fn seen(run: &Run<Answer>) -> (Vec<Value>, Vec<&str>) {
    (
        progress::run_events(None, &run.events),
        run.verdict.skipped().collect(),
    )
}

fn round(round: u32, steps: &[&str]) -> Value {
    json!({ "event": "round_ready", "round": round, "steps": steps })
}

/// A yes/no step's `step_settled`, whose level is its P(no).
fn settled(step: &str, outcome: &str, yes: f64, no: f64) -> Value {
    json!({ "event": "step_settled", "step": step,
            "answer": { "outcome": outcome, "p": { "yes": yes, "no": no }, "level": no,
                        "model": "stand-in", "placeholder": true } })
}

fn failed(step: &str, reason: &str) -> Value {
    json!({ "event": "step_settled", "step": step, "failed": reason })
}

const ASK: &str = "Does this commit message break Conventional Commits?";

/// A rule on `x` with `judges`, the yes/no ask `commit`, `steps` and
/// `then`.
fn rule_text(judges: &Value, steps: &Value, then: &Value) -> String {
    json!({
        "on": "x",
        "judges": judges,
        "asks": { "commit": { "outcomes": ["yes", "no"], "instructions": ASK } },
        "strategy": steps,
        "then": then
    })
    .to_string()
}

/// Two judges, `jev` and `glide`, each settling at 0.7.
fn bars() -> Value {
    json!({ "jev": { "tier": "system-one", "confidence": 0.7 },
            "glide": { "tier": "system-one", "confidence": 0.7 } })
}

/// `fast` asks `jev`, and `sure` asks `glide` after `need`.
fn escalation(need: &Value) -> Value {
    json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "sure", "judge": "glide", "ask": "commit", "after": [need] }
    ])
}

fn parse(text: &str) -> Rule {
    Rule::parse("r", text).unwrap()
}

/// The lowered needs of the step at `index`, all then any.
fn needs(strategy: &Strategy, index: usize) -> (Vec<Need>, Vec<Need>) {
    let step = &strategy.plan().steps()[index];

    (step.all().to_vec(), step.any().to_vec())
}

const fn yes_at(at: u16) -> Bar {
    Bar { outcome: 0, at }
}

const fn no_at(at: u16) -> Bar {
    Bar { outcome: 1, at }
}

/// The commit nudge's escalation: `sure` waits for `fast` near the bar
/// on the decision's `yes`, or failed.
fn nudge() -> Rule {
    let need = json!({ "step": "fast", "near": 0.15, "when": ["failed"] });
    let then = json!([{ "if": { "decision": "yes" }, "withhold": "event" }]);

    parse(&rule_text(&bars(), &escalation(&need), &then))
}

#[test]
fn a_near_need_asks_sure_only_near_the_bar_the_decision_acts_on() {
    let rule = nudge();
    let near = Near::new(150, [yes_at(700)]);

    assert_eq!(
        needs(rule.strategy(), 1),
        (
            vec![Need::outcome("fast", Some(near), Some(vec![When::Failed]))],
            vec![]
        )
    );

    let confident = run(rule.strategy(), &[("jev", Ok(0.94))]);
    assert_eq!(
        seen(&confident),
        (
            vec![round(1, &["fast"]), settled("fast", "yes", 0.94, 0.06)],
            vec!["sure"]
        )
    );
    assert_eq!(
        progress::verdict(&confident.verdict),
        json!({
            "fast": { "answer": { "outcome": "yes", "p": { "yes": 0.94, "no": 0.06 }, "level": 0.06,
                                  "model": "stand-in", "placeholder": true } }
        })
    );

    let near_bar = run(rule.strategy(), &[("jev", Ok(0.8)), ("glide", Ok(0.06))]);
    assert_eq!(
        seen(&near_bar),
        (
            vec![
                round(1, &["fast"]),
                settled("fast", "yes", 0.8, 0.2),
                round(2, &["sure"]),
                settled("sure", "no", 0.06, 0.94),
            ],
            vec![]
        )
    );
}

#[test]
fn a_near_need_holds_at_its_margin_and_on_a_listed_failure() {
    let rule = nudge();

    // 0.55 lies exactly 0.15 from the bar; 0.54 does not.
    let edge = run(rule.strategy(), &[("jev", Ok(0.55)), ("glide", Ok(0.9))]);
    assert_eq!(edge.verdict.skipped().count(), 0);
    let past = run(rule.strategy(), &[("jev", Ok(0.54)), ("glide", Ok(0.9))]);
    assert_eq!(
        seen(&past),
        (
            vec![round(1, &["fast"]), settled("fast", "unsure", 0.54, 0.46)],
            vec!["sure"]
        )
    );

    let conventional = run(rule.strategy(), &[("jev", Ok(0.25))]);
    assert_eq!(
        seen(&conventional),
        (
            vec![round(1, &["fast"]), settled("fast", "no", 0.25, 0.75)],
            vec!["sure"]
        )
    );

    let down = run(
        rule.strategy(),
        &[("jev", Err(Failure::Timeout)), ("glide", Ok(0.9))],
    );
    assert_eq!(
        seen(&down),
        (
            vec![
                round(1, &["fast"]),
                failed("fast", "timeout"),
                round(2, &["sure"]),
                settled("sure", "yes", 0.9, 0.1),
            ],
            vec![]
        )
    );
}

/// What `then` returns for a run: its action and payload.
fn returned(rule: &Rule, run: &Run<Answer>) -> Option<(String, Value)> {
    Then::first_met(rule.then(), &run.verdict)
        .map(|then| (then.name().to_owned(), then.payload().clone()))
}

#[test]
fn the_decision_is_the_answer_of_the_last_step_that_ran() {
    let need = json!({ "step": "fast", "near": 0.15, "when": ["failed"] });
    let then = json!([
        { "if": { "decision": "yes" }, "ask": "yes" },
        { "if": { "decision": "no" }, "ask": "no" }
    ]);
    let rule = parse(&rule_text(&bars(), &escalation(&need), &then));

    let confident = run(rule.strategy(), &[("jev", Ok(0.94))]);
    let overridden = run(rule.strategy(), &[("jev", Ok(0.8)), ("glide", Ok(0.06))]);

    assert_eq!(
        confident.verdict.decision().map(|(step, _)| step),
        Some("fast")
    );
    assert_eq!(
        returned(&rule, &confident),
        Some(("ask".to_owned(), json!("yes")))
    );

    assert_eq!(
        overridden.verdict.decision().map(|(step, _)| step),
        Some("sure")
    );
    assert_eq!(
        returned(&rule, &overridden),
        Some(("ask".to_owned(), json!("no")))
    );
}

#[test]
fn a_last_step_that_fails_is_the_decision() {
    let need = json!({ "step": "fast", "near": 0.15 });
    let then = json!([
        { "if": { "decision": "yes" }, "ask": "yes" },
        { "if": { "decision": "failed" }, "ask": "failed" }
    ]);
    let rule = parse(&rule_text(&bars(), &escalation(&need), &then));

    let ran = run(
        rule.strategy(),
        &[("jev", Ok(0.8)), ("glide", Err(Failure::Timeout))],
    );

    assert_eq!(
        seen(&ran),
        (
            vec![
                round(1, &["fast"]),
                settled("fast", "yes", 0.8, 0.2),
                round(2, &["sure"]),
                failed("sure", "timeout"),
            ],
            vec![]
        )
    );
    assert_eq!(
        ran.verdict.decision(),
        Some(("sure", &Outcome::Failed(Failure::Timeout)))
    );
    assert_eq!(
        ran.verdict.get(DECISION),
        Some(&Outcome::Failed(Failure::Timeout))
    );
    assert_eq!(
        returned(&rule, &ran),
        Some(("ask".to_owned(), json!("failed")))
    );
}

#[test]
fn a_then_on_a_step_bars_its_outcome_and_still_returns_on_it() {
    let need = json!({ "step": "fast", "near": 0.15 });
    let then = json!([{ "if": { "fast": "no" }, "withhold": "event" }]);
    let rule = parse(&rule_text(&bars(), &escalation(&need), &then));
    let near = Near::new(150, [no_at(700)]);

    assert_eq!(
        needs(rule.strategy(), 1),
        (vec![Need::outcome("fast", Some(near), None)], vec![])
    );

    let no = run(rule.strategy(), &[("jev", Ok(0.25)), ("glide", Ok(0.2))]);
    let yes = run(rule.strategy(), &[("jev", Ok(0.8))]);

    assert_eq!(no.verdict.skipped().count(), 0);
    assert_eq!(
        returned(&rule, &no),
        Some(("withhold".to_owned(), json!("event")))
    );
    assert_eq!(yes.verdict.skipped().collect::<Vec<_>>(), ["sure"]);
    assert_eq!(returned(&rule, &yes), None);
}

/// An inline strategy: `fast` asks Jev, then `steps` follow.
fn inline(steps: &[Value]) -> Value {
    let ask = json!({ "outcomes": ["yes", "no"], "instructions": ASK });
    let mut strategy = vec![json!({ "id": "fast", "judge": "jev", "ask": ask })];
    strategy.extend(steps.iter().cloned());

    json!({ "protocol": 1, "judges": bars(), "strategy": strategy })
}

fn strategy(params: &Value) -> Strategy {
    let strategy = match Navigate::parse(params).unwrap().into_target() {
        Target::Strategy(strategy) => Some(strategy),
        Target::On(_) => None,
    };

    strategy.unwrap()
}

fn ask() -> Value {
    json!({ "outcomes": ["yes", "no"], "instructions": ASK })
}

#[test]
fn an_inline_near_bars_what_a_later_when_acts_on() {
    let params = inline(&[
        json!({ "id": "sure", "judge": "glide", "ask": ask(),
                "after": [{ "step": "fast", "near": 0.15 }] }),
        json!({ "id": "tidy", "judge": "glide", "ask": ask(),
                "after": [{ "step": "fast", "when": ["no"] }] }),
    ]);
    let strategy = strategy(&params);

    assert_eq!(
        needs(&strategy, 1),
        (
            vec![Need::outcome(
                "fast",
                Some(Near::new(150, [no_at(700)])),
                None
            )],
            vec![]
        )
    );
    assert_eq!(
        needs(&strategy, 2),
        (
            vec![Need::outcome("fast", None, Some(vec![When::Outcome(1)]))],
            vec![]
        )
    );

    let no = run(&strategy, &[("jev", Ok(0.25)), ("glide", Ok(0.9))]);
    let yes = run(&strategy, &[("jev", Ok(0.8))]);

    assert_eq!(
        seen(&no).0[..3],
        [
            round(1, &["fast"]),
            settled("fast", "no", 0.25, 0.75),
            round(2, &["sure", "tidy"])
        ]
    );
    assert_eq!(
        seen(&yes),
        (
            vec![round(1, &["fast"]), settled("fast", "yes", 0.8, 0.2)],
            vec!["sure", "tidy"]
        )
    );
}

#[test]
fn an_inline_near_bars_the_outcome_it_names() {
    let params = inline(&[json!({ "id": "sure", "judge": "glide", "ask": ask(),
        "after": [{ "step": "fast", "near": 0.15, "outcome": "yes" }] })]);
    let strategy = strategy(&params);

    assert_eq!(
        needs(&strategy, 1),
        (
            vec![Need::outcome(
                "fast",
                Some(Near::new(150, [yes_at(700)])),
                None
            )],
            vec![]
        )
    );

    let near = run(&strategy, &[("jev", Ok(0.8)), ("glide", Ok(0.9))]);
    let far = run(&strategy, &[("jev", Ok(0.25))]);

    assert_eq!(near.verdict.skipped().count(), 0);
    assert_eq!(far.verdict.skipped().collect::<Vec<_>>(), ["sure"]);
}

#[test]
fn an_inline_near_with_nothing_to_bar_is_invalid() {
    let params = inline(&[json!({ "id": "sure", "judge": "glide", "ask": ask(),
        "after": [{ "step": "fast", "near": 0.15 }] })]);

    assert_eq!(
        Navigate::parse(&params).unwrap_err(),
        Rejection::Invalid(
            "rule `inline`: step `sure` waits on `fast`: `near` has no outcome to bar: name one in `outcome`, or act on one in `then` or a later `when`".to_owned()
        )
    );
}

#[test]
fn near_takes_each_outcome_bar_from_the_judge_of_the_step_it_waits_on() {
    let judges = json!({
        "careful": { "tier": "system-one", "confidence": { "yes": 0.7, "no": 0.9 } },
        "loose": { "tier": "system-one", "confidence": 0.5 }
    });
    let steps = json!([
        { "id": "fast", "judge": "careful", "ask": "commit" },
        { "id": "sure", "judge": "loose", "ask": "commit",
          "after": [{ "step": "fast", "near": 0.1 }] }
    ]);
    let then = json!([
        { "if": { "fast": "yes" }, "ask": "yes" },
        { "if": { "fast": "no" }, "ask": "no" }
    ]);
    let rule = parse(&rule_text(&judges, &steps, &then));
    let near = Near::new(100, [yes_at(700), no_at(900)]);

    assert_eq!(
        needs(rule.strategy(), 1),
        (vec![Need::outcome("fast", Some(near), None)], vec![])
    );

    let skipped = |yes: f64| {
        let ran = run(rule.strategy(), &[("careful", Ok(yes)), ("loose", Ok(0.9))]);
        ran.verdict.skipped().count() == 1
    };

    // Near 0.7 on yes, near 0.9 on no, and neither; loose's own 0.5 bar
    // would have matched 0.45.
    assert_eq!(
        [0.75, 0.15, 0.35, 0.45].map(skipped),
        [false, false, true, true]
    );
}

/// Asserts `steps` with `judges` and `then` is refused with `expected`.
fn assert_refused(judges: &Value, steps: &Value, then: &Value, expected: &str) {
    let error = Rule::parse("bad", &rule_text(judges, steps, then)).unwrap_err();

    assert_eq!(error.to_string(), expected);
}

#[test]
fn a_need_naming_what_its_step_cannot_give_is_refused() {
    let yes = json!([{ "if": { "fast": "yes" }, "ask": "yes" }]);
    let cases = [
        (
            json!({ "step": "fast", "when": ["maybe"] }),
            "`when` names `maybe`, which is not one of its outcomes",
        ),
        (
            json!({ "step": "fast", "near": 0.1, "outcome": "maybe" }),
            "`outcome` names `maybe`, which is not one of its outcomes",
        ),
        (
            json!({ "step": "fast", "outcome": "yes", "when": ["no"] }),
            "`outcome` comes only with `near`",
        ),
        (
            json!({ "step": "fast", "near": 1.5 }),
            "`near` 1.5 is not a margin from 0 to 1",
        ),
        (
            json!({ "step": "fast", "near": -0.1 }),
            "`near` -0.1 is not a margin from 0 to 1",
        ),
    ];

    for (need, reason) in cases {
        assert_refused(
            &bars(),
            &escalation(&need),
            &yes,
            &format!("rule `bad`: step `sure` waits on `fast`: {reason}"),
        );
    }
}

#[test]
fn a_near_need_without_an_authored_bar_is_refused() {
    let need = json!({ "step": "fast", "near": 0.1, "outcome": "yes" });
    let then = json!([{ "if": { "fast": "yes" }, "ask": "yes" }]);
    let tier = json!([
        { "id": "fast", "judge": "system-one", "ask": "commit" },
        { "id": "sure", "judge": "glide", "ask": "commit", "after": [need] }
    ]);
    let unset = json!({ "jev": { "tier": "system-one" }, "glide": { "tier": "system-one" } });
    let refused = |judge: &str| {
        format!(
            "rule `bad`: step `sure` waits on `fast`: `near` needs a bar, and judge `{judge}` has none: it settles on the likeliest outcome"
        )
    };

    assert_refused(&bars(), &tier, &then, &refused("system-one"));
    assert_refused(&unset, &escalation(&need), &then, &refused("jev"));
}

#[test]
fn a_near_need_bars_no_outcome_the_rule_does_not_act_on() {
    let need = json!({ "step": "fast", "near": 0.1 });
    let nothing = "rule `bad`: step `sure` waits on `fast`: `near` has no outcome to bar: name one in `outcome`, or act on one in `then` or a later `when`";

    // Only on `sure`, or only unsure and failed: nothing on `fast`.
    for then in [
        json!([{ "if": { "sure": "yes" }, "ask": "yes" }]),
        json!([{ "if": { "fast": "unsure" }, "ask": "a" }, { "if": { "fast": "failed" }, "ask": "b" }]),
    ] {
        assert_refused(&bars(), &escalation(&need), &then, nothing);
    }

    // `other` shares `fast`'s ask; acting on it bars nothing on `fast`.
    let shared = json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "other", "judge": "jev", "ask": "commit" },
        { "id": "sure", "judge": "glide", "ask": "commit", "after": [need] }
    ]);
    let on_other = json!([{ "if": { "other": "yes" }, "ask": "yes" }]);
    assert_refused(&bars(), &shared, &on_other, nothing);
}

#[test]
fn core_refuses_needs_on_what_the_plan_cannot_give() {
    let yes = json!([{ "if": { "fast": "yes" }, "ask": "yes" }]);
    let step = |id: &str, after: &Value| json!({ "id": id, "judge": "jev", "ask": "commit", "after": after });
    let cases = [
        (
            escalation(&json!({ "step": "fast", "when": [] })),
            "rule `bad`: step `sure` waits on `fast` with an empty `when`",
        ),
        (
            escalation(&json!({ "step": "fast" })),
            "rule `bad`: step `sure` waits on `fast` with neither a bar nor a `when`",
        ),
        (
            escalation(&json!({ "step": "nope", "near": 0.1 })),
            "rule `bad`: step `sure` waits on `nope`, which the plan lacks",
        ),
        (
            escalation(&json!("nope")),
            "rule `bad`: step `sure` waits on `nope`, which the plan lacks",
        ),
        (
            json!([step("fast", &json!([])), step("sure", &json!(["sure"]))]),
            "rule `bad`: step `sure` waits on `sure`, which is not an earlier step",
        ),
        (
            json!([
                step("fast", &json!([{ "step": "sure", "when": ["yes"] }])),
                step("sure", &json!([]))
            ]),
            "rule `bad`: step `fast` waits on `sure`, which is not an earlier step",
        ),
        (
            json!([
                step("fast", &json!([])),
                step("fast", &json!([])),
                step("sure", &json!(["fast"]))
            ]),
            "rule `bad`: two steps are named `fast`",
        ),
    ];

    for (steps, expected) in cases {
        assert_refused(&bars(), &steps, &yes, expected);
    }

    let typo = escalation(&json!({ "step": "fast", "nearr": 0.1 }));
    assert!(matches!(
        Rule::parse("bad", &rule_text(&bars(), &typo, &yes)),
        Err(RuleError::Parse { .. })
    ));
}

#[test]
fn a_need_on_a_later_step_or_itself_is_refused_by_core_before_its_bars() {
    let step = |id: &str, after: &Value| json!({ "id": id, "judge": "jev", "ask": "commit", "after": after });
    let near_sure = json!([{ "step": "sure", "near": 0.1 }]);
    let maybe_sure = json!([{ "step": "sure", "when": ["maybe"] }]);
    let later = "rule `bad`: step `fast` waits on `sure`, which is not an earlier step";
    let itself = "rule `bad`: step `sure` waits on `sure`, which is not an earlier step";

    // Nothing in `then` selects a bar, and `maybe` is no outcome of `sure`.
    let cases = [
        (
            json!([step("fast", &near_sure), step("sure", &json!([]))]),
            later,
        ),
        (
            json!([step("fast", &maybe_sure), step("sure", &json!([]))]),
            later,
        ),
        (
            json!([step("fast", &json!([])), step("sure", &near_sure)]),
            itself,
        ),
        (
            json!([step("fast", &json!([])), step("sure", &maybe_sure)]),
            itself,
        ),
    ];

    for (steps, expected) in cases {
        assert_refused(&bars(), &steps, &json!([]), expected);
    }
}

#[test]
fn repeated_needs_are_kept_as_written() {
    let near = json!({ "step": "fast", "near": 0.1, "outcome": "yes" });
    let steps = json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "sure", "judge": "glide", "ask": "commit", "after": ["fast", "fast", near, near] }
    ]);
    let rule = parse(&rule_text(&bars(), &steps, &json!([])));
    let near = Need::outcome("fast", Some(Near::new(100, [yes_at(700)])), None);

    assert_eq!(
        needs(rule.strategy(), 1),
        (
            vec![
                Need::settled("fast"),
                Need::settled("fast"),
                near.clone(),
                near
            ],
            vec![]
        )
    );

    let ran = run(rule.strategy(), &[("jev", Ok(0.75)), ("glide", Ok(0.9))]);
    assert_eq!(ran.verdict.skipped().count(), 0);
}

#[test]
fn a_step_with_after_and_after_any_runs_only_when_both_hold() {
    let judges = json!({ "jev": { "tier": "system-one", "confidence": 0.7 },
                         "subq": { "tier": "system-one", "confidence": 0.7 },
                         "glide": { "tier": "system-one", "confidence": 0.7 } });
    let steps = json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "mid", "judge": "subq", "ask": "commit" },
        { "id": "last", "judge": "glide", "ask": "commit",
          "after": [{ "step": "fast", "when": ["yes"] }],
          "after_any": [{ "step": "mid", "when": ["yes"] }, { "step": "fast", "when": ["failed"] }] }
    ]);
    let rule = parse(&rule_text(&judges, &steps, &json!([])));

    assert_eq!(
        needs(rule.strategy(), 2),
        (
            vec![Need::outcome("fast", None, Some(vec![When::Outcome(0)]))],
            vec![
                Need::outcome("mid", None, Some(vec![When::Outcome(0)])),
                Need::outcome("fast", None, Some(vec![When::Failed])),
            ]
        )
    );

    let last_runs = |fast: f64, mid: f64| {
        let answers = [("jev", Ok(fast)), ("subq", Ok(mid)), ("glide", Ok(0.9))];
        run(rule.strategy(), &answers).verdict.get("last").is_some()
    };

    assert!(last_runs(0.9, 0.9));
    assert!(!last_runs(0.9, 0.1), "after holds, after_any does not");
    assert!(!last_runs(0.1, 0.9), "after_any holds, after does not");
}

/// `fast` asking `ask` of a judge at 0.5, and `waiting` steps each
/// with a near need on its `o0`.
fn near_needs(ask: &Value, waiting: usize) -> String {
    let near = json!({ "step": "fast", "near": 0.1, "outcome": "o0" });
    let mut steps = vec![json!({ "id": "fast", "judge": "half", "ask": ask })];
    steps.extend((0..waiting).map(
        |index| json!({ "id": format!("w{index}"), "judge": "half", "ask": ask, "after": [near] }),
    ));

    json!({ "on": "x", "judges": { "half": { "tier": "system-one", "confidence": 0.5 } },
            "strategy": steps })
    .to_string()
}

fn outcomes(count: usize) -> Value {
    let labels: Vec<String> = (0..count).map(|index| format!("o{index}")).collect();

    json!({ "outcomes": labels, "instructions": "Which?" })
}

#[test]
fn near_needs_on_one_step_fit_its_codes_or_are_refused() {
    let full = "rule `wide`: step `fast` has no outcomes, or more outcomes and near needs than its codes hold";

    assert!(Rule::parse("wide", &near_needs(&outcomes(2), 5)).is_ok());
    assert_eq!(
        Rule::parse("wide", &near_needs(&outcomes(2), 6))
            .unwrap_err()
            .to_string(),
        full
    );

    assert!(Rule::parse("wide", &near_needs(&outcomes(62), 1)).is_ok());
    assert_eq!(
        Rule::parse("wide", &near_needs(&outcomes(63), 1))
            .unwrap_err()
            .to_string(),
        full
    );
}

const fn exceeded(limit: Limit, count: u64) -> Rejection {
    Rejection::LimitExceeded(Exceeded { limit, count })
}

#[test]
fn each_needs_list_holds_64_needs() {
    for list in ["after", "after_any"] {
        let at = |count: usize| {
            let mut sure = json!({ "id": "sure", "judge": "glide", "ask": ask() });
            sure[list] = json!(vec!["fast"; count]);

            Navigate::parse(&inline(&[sure]))
        };

        assert!(at(64).is_ok(), "{list}");
        assert_eq!(at(65).unwrap_err(), exceeded(Limit::Needs, 65), "{list}");
    }
}

#[test]
fn a_when_holds_every_outcome_unsure_and_failed() {
    let labels: Vec<String> = (0..64).map(|index| format!("o{index}")).collect();
    let mut when = labels.clone();
    when.extend(["unsure".to_owned(), "failed".to_owned()]);
    let at = |when: &[String]| {
        let ask = json!({ "outcomes": labels, "instructions": "Which?" });
        Navigate::parse(&json!({
            "protocol": 1,
            "strategy": [
                { "id": "fast", "judge": "system-one", "ask": ask },
                { "id": "sure", "judge": "system-one", "ask": ask,
                  "after": [{ "step": "fast", "when": when }] }
            ]
        }))
    };

    assert_eq!(when.len(), 66);
    assert!(at(&when).is_ok());

    when.push("failed".to_owned());
    assert_eq!(at(&when).unwrap_err(), exceeded(Limit::WhenOutcomes, 67));
}

#[test]
fn a_need_text_over_16_kib_is_refused() {
    let most = "x".repeat(16_384);
    let over = "x".repeat(16_385);
    let with = |id: &str, need: Value| {
        Navigate::parse(&inline(&[
            json!({ "id": id, "judge": "glide", "ask": ask() }),
            json!({ "id": "sure", "judge": "glide", "ask": ask(), "after": [need] }),
        ]))
    };
    let text_bytes = exceeded(Limit::TextBytes, 16_385);

    assert!(with(&most, json!(most)).is_ok());
    assert!(with(&most, json!({ "step": most, "when": ["yes"] })).is_ok());

    assert_eq!(with("a", json!(over)).unwrap_err(), text_bytes);
    assert_eq!(
        with("a", json!({ "step": over, "when": ["yes"] })).unwrap_err(),
        text_bytes
    );
    assert_eq!(
        with("a", json!({ "step": "a", "when": [over] })).unwrap_err(),
        text_bytes
    );
    assert_eq!(
        with("a", json!({ "step": "a", "near": 0.1, "outcome": over })).unwrap_err(),
        text_bytes
    );
}

#[test]
fn an_outcome_named_skipped_is_a_real_outcome() {
    let ask = json!({ "outcomes": ["shipped", "skipped"], "instructions": "Shipped?" });
    let text = json!({
        "on": "x",
        "judges": bars(),
        "strategy": [
            { "id": "fast", "judge": "jev", "ask": ask },
            { "id": "sure", "judge": "glide", "ask": ask,
              "after": [{ "step": "fast", "when": ["skipped"] }] },
            { "id": "tidy", "judge": "glide", "ask": ask,
              "after": [{ "step": "fast", "near": 0.2 }] }
        ],
        "then": [{ "if": { "fast": "skipped" }, "withhold": "event" }]
    })
    .to_string();
    let rule = parse(&text);
    let near = Near::new(
        200,
        [Bar {
            outcome: 1,
            at: 700,
        }],
    );

    assert_eq!(
        needs(rule.strategy(), 1),
        (
            vec![Need::outcome("fast", None, Some(vec![When::Outcome(1)]))],
            vec![]
        )
    );
    assert_eq!(
        needs(rule.strategy(), 2),
        (vec![Need::outcome("fast", Some(near), None)], vec![])
    );

    let ran = run(rule.strategy(), &[("jev", Ok(0.1)), ("glide", Ok(0.9))]);
    assert_eq!(ran.verdict.skipped().count(), 0);
    assert_eq!(
        returned(&rule, &ran),
        Some(("withhold".to_owned(), json!("event")))
    );
}

#[test]
fn a_then_on_the_decision_expects_an_outcome_some_step_has() {
    let steps = json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "size", "judge": "glide",
          "ask": { "outcomes": ["small", "large"], "instructions": "How big?" } }
    ]);
    let then = |wanted: &str| json!([{ "if": { "decision": wanted }, "ask": "x" }]);

    for wanted in ["yes", "large", "unsure", "failed"] {
        assert!(
            Rule::parse("good", &rule_text(&bars(), &steps, &then(wanted))).is_ok(),
            "{wanted}"
        );
    }
    assert_refused(
        &bars(),
        &steps,
        &then("maybe"),
        "rule `bad`: then entry 0 expects `maybe` from `decision`, which no step has as an outcome",
    );
}

const RESERVED: &str = "step ID `decision` is reserved for the step that decides the run";

#[test]
fn a_step_named_decision_is_refused() {
    let plain = json!([
        { "id": "fast", "judge": "jev", "ask": "commit" },
        { "id": "decision", "judge": "glide", "ask": "commit", "after": ["fast"] }
    ]);
    let on_decision = json!([{ "if": { "decision": "yes" }, "ask": "yes" }]);
    // Read as a step, the near need would have no outcome to bar.
    let waited_on = json!([
        { "id": "decision", "judge": "jev", "ask": "commit" },
        { "id": "sure", "judge": "glide", "ask": "commit",
          "after": [{ "step": "decision", "near": 0.1 }] }
    ]);

    for (steps, then) in [(plain, on_decision), (waited_on, json!([]))] {
        assert_refused(&bars(), &steps, &then, &format!("rule `bad`: {RESERVED}"));
    }
}

#[test]
fn an_inline_step_named_decision_is_invalid() {
    let params = inline(&[json!({ "id": "decision", "judge": "glide", "ask": ask(),
        "after": ["fast"] })]);

    assert_eq!(
        Navigate::parse(&params).unwrap_err(),
        Rejection::Invalid(format!("rule `inline`: {RESERVED}"))
    );
}

#[test]
fn an_inline_need_error_is_invalid() {
    let params = inline(&[json!({ "id": "sure", "judge": "glide", "ask": ask(),
        "after": [{ "step": "fast", "near": 2 }] })]);

    assert_eq!(
        Navigate::parse(&params).unwrap_err(),
        Rejection::Invalid(
            "rule `inline`: step `sure` waits on `fast`: `near` 2 is not a margin from 0 to 1"
                .to_owned()
        )
    );
}
