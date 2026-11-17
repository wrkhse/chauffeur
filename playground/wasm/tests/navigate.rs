//! The navigate tab's function, run natively with requests like the page's.
#![expect(
    clippy::unwrap_used,
    reason = "a reply that is not JSON fails the test"
)]

use chauffeur_playground::navigate;
use serde_json::{Value, json};

fn navigated(input: &Value) -> Value {
    serde_json::from_str(&navigate(&input.to_string())).unwrap()
}

#[test]
fn browser_examples_match_the_schema_and_expected_results() {
    let cases: Value =
        serde_json::from_str(include_str!("../../src/features/navigate-examples.json")).unwrap();
    let schema: Value = serde_json::from_str(include_str!(
        "../../../crates/protocol/schema/navigate.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut count = 0;

    for (name, case) in cases.as_object().unwrap() {
        count += 1;
        let input = &case["input"];
        assert_eq!(
            validator.is_valid(&input["request"]),
            case["valid"].as_bool().unwrap(),
            "{name}"
        );
        let reply = navigated(input);
        assert_eq!(
            reply.pointer(case["pointer"].as_str().unwrap()),
            Some(&case["expected"]),
            "{name}: {reply}"
        );
    }

    // The seven examples the page's picker offers, so one added or dropped
    // shows up here as well as on the page.
    assert_eq!(count, 7);
}

fn triage() -> Value {
    json!({
        "on": "sourcefed.event",
        "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
        "strategy": [{ "id": "screen", "judge": "quick",
                       "ask": { "outcomes": ["yes", "no"], "evidence": ["event"], "instructions": "Show it?" } }],
        "then": [{ "if": { "screen": "no" }, "withhold": "event" }]
    })
}

/// An event for the triage rule, with `screen` as its step's reply, or none.
fn event(text: &str, screen: &Value) -> Value {
    let answers = if screen.is_null() {
        json!({})
    } else {
        json!({ "screen": screen })
    };
    json!({
        "request": { "protocol": 1, "on": "sourcefed.event", "evidence": { "event": text } },
        "rules": { "sourcefed/triage": triage() },
        "answers": answers
    })
}

#[test]
fn confident_noise_is_withheld_after_the_runs_events() {
    let reply = navigated(&event("thanks", &json!({ "yes": 0.06, "no": 0.94 })));

    let kinds: Vec<&Value> = reply["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| &event["event"])
        .collect();
    assert_eq!(
        kinds,
        [
            &json!("rule_matched"),
            &json!("round_ready"),
            &json!("step_settled")
        ]
    );
    assert_eq!(
        reply["result"],
        json!({ "intents": [{ "rule": "sourcefed/triage", "withhold": "event" }] })
    );
}

#[test]
fn all_matches_precede_runs_in_rule_order() {
    let mut input = event("thanks", &json!({ "yes": 0.06, "no": 0.94 }));
    input["rules"] = json!({ "a": triage(), "b": triage() });
    let reply = navigated(&input);
    let events = reply["events"].as_array().unwrap();
    let sequence: Vec<(&str, Option<&str>)> = events
        .iter()
        .map(|event| (event["event"].as_str().unwrap(), event["rule"].as_str()))
        .collect();
    assert_eq!(
        sequence,
        [
            ("rule_matched", Some("a")),
            ("rule_matched", Some("b")),
            ("round_ready", Some("a")),
            ("step_settled", Some("a")),
            ("round_ready", Some("b")),
            ("step_settled", Some("b")),
        ]
    );
    assert_eq!(reply["result"]["intents"].as_array().unwrap().len(), 2);
}

#[test]
fn distinct_steps_receive_their_own_replies() {
    let reply = navigated(&json!({
        "request": { "protocol": 1, "strategy": [
            { "id": "first", "judge": "system-one", "ask": { "outcomes": ["yes", "no"], "instructions": "Same question?" } },
            { "id": "second", "judge": "system-one", "ask": { "outcomes": ["yes", "no"], "instructions": "Same question?" } }
        ] },
        "answers": { "first": { "yes": 0.9, "no": 0.1 }, "second": { "yes": 0.1, "no": 0.9 } }
    }));
    assert_eq!(
        reply["result"]["verdict"]["first"]["answer"]["outcome"], "yes",
        "{reply}"
    );
    assert_eq!(
        reply["result"]["verdict"]["second"]["answer"]["outcome"], "no",
        "{reply}"
    );
}

#[test]
fn invalid_answers_report_the_step_and_expected_shape_or_failure_names() {
    let invalid = navigated(&event("x", &json!(42)));
    assert_eq!(
        invalid["error"],
        "answers.screen: expected {outcome:p,...} or {failed:name}"
    );
    let invalid = navigated(&event("x", &json!({ "failed": "typo" })));
    let error = invalid["error"].as_str().unwrap();
    for name in [
        "screen",
        "typo",
        "unavailable",
        "rate_limited",
        "timeout",
        "invalid",
        "deadline",
    ] {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn an_answer_keyed_to_no_step_is_an_error() {
    let mut input = event("thanks", &json!({ "yes": 0.06, "no": 0.94 }));
    input["answers"]["scrren"] = json!({ "yes": 0.06, "no": 0.94 });

    let reply = navigated(&input);

    assert_eq!(reply["error"], "answers.scrren: no such step");
}

#[test]
fn unsure_a_failure_and_no_reply_give_no_intent() {
    for screen in [
        json!({ "yes": 0.48, "no": 0.52 }),
        json!({ "failed": "timeout" }),
        Value::Null,
    ] {
        let reply = navigated(&event("which doc?", &screen));

        assert_eq!(reply["result"], json!({ "intents": [] }), "{screen}");
    }
}

#[test]
fn an_inline_strategy_gives_its_verdict_and_a_rule_that_does_not_load_is_refused() {
    let reply = navigated(&json!({
        "request": { "protocol": 1, "strategy": [{ "id": "done", "judge": "system-one",
                     "ask": { "outcomes": ["yes", "no"], "instructions": "Finished?" } }] },
        "answers": { "done": { "yes": 0.9, "no": 0.1 } }
    }));
    assert_eq!(
        reply["result"]["verdict"]["done"]["answer"]["outcome"],
        "yes"
    );

    let refused = navigated(&json!({
        "request": { "protocol": 1, "on": "x" },
        "rules": { "broken": { "on": "x", "strategy": [] } }
    }));
    assert!(
        refused["refused"][0]
            .as_str()
            .unwrap()
            .contains("rule `broken`"),
        "{refused}"
    );
}

#[test]
fn a_request_over_a_limit_is_rejected_as_a_caller_sees_it() {
    let reply =
        navigated(&json!({ "request": { "protocol": 1, "on": "x", "deadline_ms": 30_001 } }));

    assert_eq!(reply["rejected"]["data"]["limit"], "deadline_ms");
    assert_eq!(reply["rejected"]["data"]["reason"], "limit_exceeded");
    assert_eq!(reply["rejected"]["data"]["max"], 30_000);
    assert!(navigated(&json!({ "nope": 1 }))["error"].is_string());
}

#[test]
fn a_misspelled_key_names_the_field_the_input_does_not_have() {
    let reply = navigated(&json!({
        "request": { "protocol": 1, "on": "sourcefed.event", "evidence": { "event": "x" } },
        "answer": { "screen": { "yes": 0.9, "no": 0.1 } }
    }));

    let error = reply["error"].as_str().unwrap();
    assert!(error.contains("unknown field `answer`"), "{error}");
}

/// The daemon rejects the request before it reads any answer, so a caller
/// sees `unsupported_protocol`, not a malformed answer it would never use.
#[test]
fn a_rejection_wins_over_a_malformed_answer() {
    let reply = navigated(&json!({
        "request": { "protocol": 2 },
        "answers": { "x": 42 }
    }));

    assert_eq!(reply["rejected"]["data"]["reason"], "unsupported_protocol");
}
