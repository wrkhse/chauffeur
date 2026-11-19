#![expect(
    clippy::unwrap_used,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::path::PathBuf;

use chauffeur_core::{Bar, Failure, Near, Outcome, Panel, Run, Wants, When};
use chauffeur_judges::{Answer, AnsweredBy, Probabilities, Question, Tier};
use chauffeur_protocol::{Event, Expect, Rule, RuleError, Then};
use serde_json::Value;

fn shipped(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../distribution/rules")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn commit_rule() -> Rule {
    Rule::parse(
        "conventional-commits",
        &shipped("conventional-commits.json"),
    )
    .unwrap()
}

fn tool_after(command: &str) -> Event {
    let event = serde_json::json!({
        "event": "tool.after", "session": "s1", "tool": "shell", "input": { "command": command }
    });
    Event::parse(&event.to_string()).unwrap()
}

/// Runs the rule with each named judge giving `yes` with its probability,
/// settled under the rule's own confidence, or failing. A judge not named is
/// unavailable.
fn run_with(rule: &Rule, replies: &[(&str, Result<f64, Failure>)]) -> Run<Answer> {
    let judges: Vec<_> = rule
        .judges()
        .iter()
        .map(|(name, spec)| {
            let reply = replies
                .iter()
                .find(|(answering, _)| answering == name)
                .map_or(Err(Failure::Unavailable), |(_, reply)| *reply);
            let confidence = spec.confidence.clone();
            let by = AnsweredBy {
                model: name.clone(),
                placeholder: true,
            };
            let judge = move |question: &Question| match reply {
                Ok(yes) => {
                    let p =
                        Probabilities::read(question.outcomes(), [("yes", yes), ("no", 1.0 - yes)])
                            .unwrap();

                    Outcome::Value(Answer::settle(question, p, &confidence, by.clone()))
                }
                Err(failure) => Outcome::Failed(failure),
            };

            (name.as_str(), judge)
        })
        .collect();
    let panel = judges.iter().fold(Panel::new(), |panel, (name, judge)| {
        panel.with(*name, judge)
    });

    rule.plan().run(&panel).unwrap()
}

/// What the rule returns when its judges reply as in [`run_with`].
fn then_for(rule: &Rule, replies: &[(&str, Result<f64, Failure>)]) -> Option<String> {
    let run = run_with(rule, replies);

    Then::first_met(rule.then(), &run.verdict)
        .map(|then| format!("{} {}", then.name(), then.payload()))
}

#[test]
fn the_shipped_commit_rule_hooks_only_commits() {
    let rule = commit_rule();
    let read = r#"{ "event": "tool.after", "tool": "read", "input": { "path": "git commit" } }"#;
    let before =
        r#"{ "event": "tool.before", "tool": "shell", "input": { "command": "git commit" } }"#;

    assert!(rule.hooks(&tool_after("git commit -m 'fixed stuff'")));
    assert!(!rule.hooks(&tool_after("ls -la")));
    assert!(!rule.hooks(&tool_after("cargo test")));
    assert!(
        !rule.hooks(&Event::parse(read).unwrap()),
        "only shell calls"
    );
    assert!(
        !rule.hooks(&Event::parse(before).unwrap()),
        "only after the call"
    );
    assert_eq!(
        rule.cooldown().map(|cooldown| cooldown.as_secs()),
        Some(120)
    );

    let judges: Vec<_> = rule
        .judges()
        .iter()
        .map(|(name, judge)| (name.as_str(), judge.tier, judge.model.as_deref()))
        .collect();
    assert_eq!(
        judges,
        [
            ("glide", Tier::SystemOne, Some("glide")),
            ("jev", Tier::SystemOne, Some("jev")),
        ]
    );
}

/// The shipped nudge escalates from Jev to `GLiDE` near its bar or on a
/// failure, and acts on the decision plus a failed `GLiDE` after Jev's yes.
#[test]
fn the_shipped_commit_rule_escalates_near_the_bar_or_on_a_failure() {
    let rule = commit_rule();
    let steps = rule.plan().steps();

    assert_eq!(
        steps
            .iter()
            .map(|step| (step.id(), step.judge()))
            .collect::<Vec<_>>(),
        [("nonconventional", "jev"), ("sure", "glide")]
    );

    let sure = &steps[1];
    assert_eq!(sure.any(), []);
    let needs = sure.all();
    assert_eq!(needs.len(), 1);
    assert_eq!(needs[0].step(), "nonconventional");
    assert_eq!(
        needs[0].wants(),
        &Wants::Outcome {
            near: Some(Near::new(
                150,
                [Bar {
                    outcome: 0,
                    at: 700
                }]
            )),
            when: Some(vec![When::Failed]),
        }
    );

    let then = rule.then();
    assert_eq!(then.len(), 2);
    assert_eq!(
        then[0].needs(),
        &[("decision".to_owned(), Expect::Outcome("yes".to_owned()))]
    );
    assert_eq!(
        then[1].needs(),
        &[
            (
                "nonconventional".to_owned(),
                Expect::Outcome("yes".to_owned())
            ),
            ("sure".to_owned(), Expect::Failed),
        ]
    );
    assert_eq!(then[0].name(), "message.append");
    assert_eq!(then[1].name(), "message.append");
}

/// The shipped nudge, as `then_for` prints it.
fn nudge() -> String {
    let payload: Value = serde_json::from_str(
        r#"{ "label": "commit message isn't conventional; asking the agent to amend it",
             "parts": [{ "text": "Chauffeur, which the user set up to check commits: this message does not follow Conventional Commits. Unless the user chose the message, amend it to `type(scope): description`, such as `fix(parser): handle empty input`." }],
             "delivery": "now" }"#,
    )
    .unwrap();

    format!("message.append {payload}")
}

#[test]
fn a_confident_jev_decides_and_glide_is_skipped() {
    let rule = commit_rule();
    let glide_says_no = ("glide", Ok(0.06));

    let confident = run_with(&rule, &[("jev", Ok(0.94)), glide_says_no]);
    assert_eq!(confident.verdict.skipped().collect::<Vec<_>>(), ["sure"]);
    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.94)), glide_says_no]),
        Some(nudge())
    );

    let conventional = run_with(&rule, &[("jev", Ok(0.06)), ("glide", Ok(0.94))]);
    assert_eq!(conventional.verdict.skipped().collect::<Vec<_>>(), ["sure"]);
    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.06)), ("glide", Ok(0.94))]),
        None,
        "a conventional message"
    );
}

#[test]
fn glide_settles_a_yes_near_the_bar() {
    let rule = commit_rule();

    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.8)), ("glide", Ok(0.06))]),
        None,
        "glide overturns a near yes"
    );
    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.8)), ("glide", Ok(0.94))]),
        Some(nudge()),
        "glide agrees"
    );
    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.8)), ("glide", Err(Failure::Timeout))]),
        Some(nudge()),
        "a failed glide leaves jev's near yes standing"
    );
}

#[test]
fn glide_answers_when_jev_is_unsure_or_fails() {
    let rule = commit_rule();

    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.58)), ("glide", Ok(0.94))]),
        Some(nudge()),
        "glide settles an unsure jev"
    );
    assert_eq!(
        then_for(&rule, &[("jev", Ok(0.58))]),
        None,
        "unsure with glide down sends nothing"
    );
    assert_eq!(
        then_for(
            &rule,
            &[("jev", Err(Failure::Timeout)), ("glide", Ok(0.94))]
        ),
        Some(nudge()),
        "glide stands in for a failed jev"
    );
    assert_eq!(then_for(&rule, &[]), None, "both failing sends nothing");
}

#[test]
fn a_tool_event_gives_its_call_and_output_as_evidence() {
    let event = Event::parse(
        r#"{ "event": "tool.after", "session": "s1", "tool": "shell",
             "input": { "command": "git commit -m 'fixed stuff'", "timeout": 30 },
             "output": "1 file changed" }"#,
    )
    .unwrap();

    let call = "tool: shell\ncommand: git commit -m 'fixed stuff'\ntimeout: 30\n";
    assert_eq!(
        event.evidence(),
        [
            ("tool_call".to_owned(), call.to_owned()),
            ("tool_output".to_owned(), "1 file changed".to_owned()),
            ("session".to_owned(), "s1".to_owned()),
        ]
    );
    assert_eq!(event.session(), Some("s1"));
    assert!(Event::parse(r#"{ "tool": "shell" }"#).is_err());
}

const ASK: &str = r#"{ "outcomes": ["yes", "no"], "instructions": "Is it?" }"#;

/// A rule with one step `s` asking `ask` of `judge`, and the given `judges`
/// and `then`.
fn rule_text(judges: &str, judge: &str, ask: &str, then: &str) -> String {
    format!(
        r#"{{ "on": "x", "judges": {{ {judges} }},
             "strategy": [{{ "id": "s", "judge": "{judge}", "ask": {ask} }}], "then": [{then}] }}"#
    )
}

fn assert_refused(text: &str, expected: &str) {
    let error = Rule::parse("bad", text).unwrap_err().to_string();
    assert!(error.starts_with("rule `bad`"), "{error}");
    assert!(error.contains(expected), "{error} lacks {expected}");
}

#[test]
fn a_step_with_an_unknown_judge_ask_or_bad_question_is_refused() {
    assert_refused(
        &rule_text("", "nobody", ASK, ""),
        "step `s` names judge `nobody`",
    );
    assert_refused(
        &rule_text("", "system-one", r#""missing""#, ""),
        "names ask `missing`",
    );
    let one_outcome = r#"{ "outcomes": ["yes"], "instructions": "?" }"#;
    assert_refused(
        &rule_text("", "system-one", one_outcome, ""),
        "2 to 64 outcomes",
    );
    assert_refused(r#"{ "on": "x", "strategy": [] }"#, "at least one step");
    assert_refused(
        &rule_text("", "system-one", ASK, "").replace("\"then\"", "\"typo\""),
        "unknown field `typo`",
    );
    assert!(matches!(
        Rule::parse("bad", "not json"),
        Err(RuleError::Parse { .. })
    ));
}

#[test]
fn a_confidence_that_does_not_fit_is_refused() {
    let over = r#""q": { "tier": "system-one", "confidence": 1.5 }"#;
    let partial = r#""q": { "tier": "system-one", "confidence": { "yes": 0.6 } }"#;

    assert_refused(
        &rule_text(over, "q", ASK, ""),
        "confidence 1.5 is not a probability",
    );
    assert_refused(
        &rule_text(partial, "q", ASK, ""),
        "no bar for the outcome `no`",
    );
}

#[test]
fn a_then_entry_naming_what_the_strategy_lacks_is_refused() {
    let maybe = r#"{ "if": { "s": "maybe" }, "withhold": "event" }"#;
    let other_step = r#"{ "if": { "t": "yes" }, "withhold": "event" }"#;
    let nothing = r#"{ "if": { "s": "yes" } }"#;

    assert_refused(
        &rule_text("", "system-one", ASK, maybe),
        "expects `maybe` from step `s`",
    );
    assert_refused(
        &rule_text("", "system-one", ASK, other_step),
        "names step `t`",
    );
    assert_refused(&rule_text("", "system-one", ASK, nothing), "exactly one");
}

/// A rule whose `then` appends `payload` on a `yes`.
fn appending(payload: &str) -> String {
    let then = format!(r#"{{ "if": {{ "s": "yes" }}, "message.append": {payload} }}"#);

    rule_text("", "system-one", ASK, &then)
}

#[test]
fn a_message_append_with_a_label_and_each_delivery_loads() {
    for delivery in ["now", "next", "wake"] {
        let payload = format!(
            r#"{{ "label": "commit message isn't conventional", "parts": [{{ "text": "Amend it." }}],
                 "delivery": "{delivery}" }}"#
        );

        assert!(
            Rule::parse("good", &appending(&payload)).is_ok(),
            "{delivery}"
        );
    }

    let unlabelled = r#"{ "parts": [{ "text": "Amend it." }], "delivery": "now" }"#;
    assert!(Rule::parse("good", &appending(unlabelled)).is_ok());

    let longest = "x".repeat(100);
    let payload = format!(
        r#"{{ "label": "{longest}", "parts": [{{ "text": "Amend it." }}], "delivery": "now" }}"#
    );
    assert!(Rule::parse("good", &appending(&payload)).is_ok());
}

#[test]
fn a_message_append_that_a_harness_could_not_show_is_refused() {
    let text = r#"[{ "text": "Amend it." }]"#;
    let with = |fields: &str| appending(&format!("{{ {fields} }}"));

    assert_refused(&appending(r#""now""#), "needs an object");
    assert_refused(
        &with(&format!(
            r#""parts": {text}, "delivery": "now", "lable": "x""#
        )),
        "has `lable`",
    );
    assert_refused(&with(r#""parts": [], "delivery": "now""#), "needs `parts`");
    assert_refused(&with(r#""delivery": "now""#), "needs `parts`");
    assert_refused(
        &with(r#""parts": [{ "text": "" }], "delivery": "now""#),
        "part 0 of",
    );
    assert_refused(
        &with(r#""parts": [{ "text": "a", "skill": "b" }], "delivery": "now""#),
        "part 0 of",
    );
    assert_refused(&with(&format!(r#""parts": {text}"#)), "needs a `delivery`");
    assert_refused(
        &with(&format!(r#""parts": {text}, "delivery": "later""#)),
        "needs a `delivery`",
    );

    let too_long = "x".repeat(101);
    for label in [
        "\"\"",
        "\"  \"",
        "\"two\\nlines\"",
        "3",
        &format!("\"{too_long}\""),
    ] {
        assert_refused(
            &with(&format!(
                r#""label": {label}, "parts": {text}, "delivery": "now""#
            )),
            "one line of 1 to 100 characters",
        );
    }
}

#[test]
fn unsure_and_failed_can_be_expected_in_then() {
    let quick = r#""quick": { "tier": "system-one", "confidence": 0.7 }"#;
    let then = r#"{ "if": { "s": "unsure" }, "ask": "unsure" }, { "if": { "s": "failed" }, "ask": "failed" }"#;
    let rule = Rule::parse("fallbacks", &rule_text(quick, "quick", ASK, then)).unwrap();

    assert_eq!(
        then_for(&rule, &[("quick", Ok(0.5))]).as_deref(),
        Some(r#"ask "unsure""#)
    );
    assert_eq!(
        then_for(&rule, &[("quick", Err(Failure::Timeout))]).as_deref(),
        Some(r#"ask "failed""#)
    );
    assert_eq!(then_for(&rule, &[("quick", Ok(0.9))]), None);
}
