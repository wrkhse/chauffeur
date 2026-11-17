//! The `navigate` fixtures every binding replays, and the wire shapes of
//! intents, rejections and a run's events.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::path::PathBuf;

use chauffeur_core::{Failure, Outcome, Panel};
use chauffeur_judges::{Answer, AnsweredBy, Confidence, Milli, Probabilities, Question};
use chauffeur_protocol::{
    DEFAULT_DEADLINE, Intent, Limit, Navigate, Rejection, Rule, Target, progress,
};
use serde_json::{Value, json};

fn crate_file(path: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The fixture cases, each with its `fill` applied: `text` repeated
/// `times`, put at the JSON pointer `at` in its params.
fn cases() -> Vec<Value> {
    let mut cases = crate_file("fixtures/navigate.json")["cases"]
        .as_array()
        .unwrap()
        .clone();
    for case in &mut cases {
        for fill in case["fill"].as_array().cloned().unwrap_or_default() {
            let at = fill["at"].as_str().unwrap();
            let (parent, key) = at.rsplit_once('/').unwrap();
            let times = usize::try_from(fill["times"].as_u64().unwrap()).unwrap();
            let text = fill["text"].as_str().unwrap().repeat(times);
            let into = case["params"].pointer_mut(parent).unwrap();
            if let Value::Array(items) = into {
                items[key.parse::<usize>().unwrap()] = json!(text);
            } else {
                into.as_object_mut()
                    .unwrap()
                    .insert(key.to_owned(), json!(text));
            }
        }
    }
    cases
}

/// What an accepted request must keep: its deadline, its project, its
/// evidence and candidates, and for a strategy its step IDs and judges.
fn assert_kept(name: &str, params: &Value, navigate: &Navigate) {
    let deadline = params["deadline_ms"].as_u64().unwrap_or(3_000);
    assert_eq!(
        navigate.deadline().as_millis(),
        u128::from(deadline),
        "{name}"
    );
    assert_eq!(
        navigate.project(),
        params["project"].as_str().map(std::path::Path::new),
        "{name}"
    );
    let blocks = |field: &str| -> Vec<(String, String)> {
        let mut blocks: Vec<(String, String)> =
            params[field].as_object().map_or_default(|fields| {
                fields
                    .iter()
                    .map(|(name, text)| (name.clone(), text.as_str().unwrap().to_owned()))
                    .collect()
            });
        blocks.sort();
        blocks
    };
    assert_eq!(navigate.evidence(), blocks("evidence"), "{name}");
    assert_eq!(navigate.candidates(), blocks("candidates"), "{name}");
    if let Target::Strategy(strategy) = navigate.target() {
        let sent: Vec<&str> = params["strategy"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["id"].as_str().unwrap())
            .collect();
        let kept: Vec<&str> = strategy
            .plan()
            .steps()
            .iter()
            .map(chauffeur_core::Step::id)
            .collect();
        assert_eq!(kept, sent, "{name}");
        for judge in params["judges"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, _)| name)
        {
            assert!(strategy.judges().contains_key(judge), "{name}: {judge}");
        }
    }
}

#[test]
fn every_fixture_is_accepted_or_rejected_as_it_expects() {
    let cases = cases();
    assert_eq!(cases.len(), 77);

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let parsed = Navigate::parse(&case["params"]);
        match (&case["expect"], &case["reject"], parsed) {
            (Value::String(expect), Value::Null, Ok(navigate)) => {
                let kind = match navigate.target() {
                    Target::On(_) => "on",
                    Target::Strategy(_) => "strategy",
                };
                assert_eq!(kind, expect, "{name}");
                assert_kept(name, &case["params"], &navigate);
            }
            (Value::Null, reject, Err(rejection)) => {
                let error = rejection.to_error();
                assert_eq!(error["data"]["reason"], reject["reason"], "{name}: {error}");
                if let Some(limit) = reject.get("limit") {
                    assert_eq!(&error["data"]["limit"], limit, "{name}: {error}");
                }
            }
            (_, _, outcome) => panic!("{name}: {outcome:?}"),
        }
    }
}

#[test]
fn the_schema_accepts_and_rejects_every_fixture_as_the_daemon_does() {
    let schema = jsonschema::draft202012::new(&crate_file("schema/navigate.json")).unwrap();

    for case in cases() {
        let name = case["name"].as_str().unwrap();
        let accepted = case.get("expect").is_some() || case["schema"] == "accepts";
        let errors: Vec<String> = schema
            .iter_errors(&case["params"])
            .map(|error| error.to_string())
            .collect();
        assert_eq!(errors.is_empty(), accepted, "{name}: {errors:?}");
    }
}

#[test]
fn every_shipped_rule_loads_within_the_limits() {
    let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../distribution/rules");
    let mut loaded = 0;

    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        let id = path.file_stem().unwrap().to_string_lossy().into_owned();
        Rule::parse(&id, &std::fs::read_to_string(&path).unwrap()).unwrap();
        loaded += 1;
    }

    assert!(loaded > 0);
}

#[test]
fn every_limit_has_a_fixture_over_it() {
    let rejected: Vec<Value> = cases()
        .iter()
        .filter_map(|case| case["reject"].get("limit").cloned())
        .collect();

    for limit in Limit::ALL {
        assert!(rejected.contains(&json!(limit.name())), "{}", limit.name());
    }
}

#[test]
fn a_rejection_names_its_reason_and_limit() {
    let params = json!({ "protocol": 1, "on": "x", "evidence": { "log": "a".repeat(262_145) } });

    let error = Navigate::parse(&params).unwrap_err().to_error();

    assert_eq!(
        error,
        json!({ "code": -32602, "message": "evidence is over 256 KiB",
                "data": { "reason": "limit_exceeded", "limit": "evidence_bytes", "max": 262_144 } })
    );
    let protocol = Navigate::parse(&json!({ "protocol": 9, "on": "x" })).unwrap_err();
    assert_eq!(protocol.to_error()["data"]["accepted"], json!([1]));
    assert_eq!(Rejection::Busy.to_error()["code"], -32000);
}

/// Asserts `params` is refused for a named text over 16 KiB.
fn assert_text_bytes(params: &Value) {
    let rejection = Navigate::parse(params).unwrap_err();
    let error = rejection.to_error();

    assert_eq!(error["data"]["reason"], json!("limit_exceeded"), "{error}");
    assert_eq!(error["data"]["limit"], json!("text_bytes"), "{error}");
}

/// A judge's name is a key in `judges`, so no fixture's fill can reach it.
#[test]
fn a_judge_name_over_16_kib_is_refused() {
    let long = "x".repeat(16_385);
    let mut params = json!({
        "protocol": 1,
        "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
        "strategy": [{
            "id": "done", "judge": "quick",
            "ask": { "outcomes": ["yes", "no"], "instructions": "?" }
        }]
    });
    params["judges"]
        .as_object_mut()
        .unwrap()
        .insert(long, json!({ "tier": "system-one" }));

    assert_text_bytes(&params);
}

/// A step's named ask is checked before `asks` is consulted: a name over
/// 16 KiB is refused even when no ask holds it.
#[test]
fn an_ask_name_over_16_kib_is_refused() {
    let long = "x".repeat(16_385);
    let mut params = json!({
        "protocol": 1,
        "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
        "asks": {
            "finished": { "outcomes": ["yes", "no"], "evidence": ["log"], "instructions": "?" }
        },
        "strategy": [{ "id": "done", "judge": "quick", "ask": "finished" }],
        "evidence": { "log": "…" }
    });
    params["strategy"][0]["ask"] = json!(long);

    assert_text_bytes(&params);
}

/// A per-outcome confidence names each outcome as a key, bounded like every
/// other text before its bar is read.
#[test]
fn a_per_outcome_confidence_key_over_16_kib_is_refused() {
    let long = "x".repeat(16_385);
    let mut confidence = serde_json::Map::new();
    confidence.insert(long, json!(0.9));
    confidence.insert("no".to_owned(), json!(0.95));
    let params = json!({
        "protocol": 1,
        "judges": {
            "careful": { "tier": "system-one", "confidence": confidence }
        },
        "strategy": [{
            "id": "done", "judge": "careful",
            "ask": { "outcomes": ["yes", "no"], "instructions": "?" }
        }]
    });

    assert_text_bytes(&params);
}

#[test]
fn a_null_is_named_by_its_pointer() {
    let inline = |judges: Value, outcomes: Value| {
        json!({
            "protocol": 1,
            "judges": judges,
            "strategy": [{
                "id": "done", "judge": "quick",
                "ask": { "outcomes": outcomes, "evidence": ["log"],
                         "instructions": "Is the migration finished?" }
            }],
            "evidence": { "log": "migrated 42 of 42 tables" }
        })
    };

    let cases = [
        (
            json!({ "protocol": 1, "on": "x", "deadline_ms": null }),
            "`/deadline_ms`",
        ),
        (
            inline(
                json!({ "quick": { "tier": "system-one", "confidence": 0.7, "model": null } }),
                json!(["yes", "no"]),
            ),
            "`/judges/quick/model`",
        ),
        (
            inline(
                json!({ "quick": { "tier": "system-one", "confidence": 0.7 } }),
                json!(["yes", null]),
            ),
            "`/strategy/0/ask/outcomes/1`",
        ),
        (
            json!({ "protocol": 1, "on": "x", "evidence": { "a/b~c": null } }),
            "`/evidence/a~1b~0c`",
        ),
    ];

    for (params, pointer) in cases {
        let rejection = Navigate::parse(&params).unwrap_err();

        assert_eq!(
            rejection.to_error()["data"]["reason"],
            json!("invalid"),
            "{pointer}"
        );
        assert!(rejection.to_string().contains(pointer), "{rejection}");
    }
}

#[test]
fn an_event_request_matches_rules_by_its_on_and_evidence() {
    let navigate = Navigate::parse(&json!({
        "protocol": 1, "on": "sourcefed.event", "evidence": { "event": "CI failed" }
    }))
    .unwrap();
    let rule = Rule::parse(
        "sourcefed/triage",
        r#"{ "on": "sourcefed.event", "strategy": [{ "id": "screen", "judge": "system-one",
             "ask": { "outcomes": ["yes", "no"], "evidence": ["event"], "instructions": "Show it?" } }],
             "then": [{ "if": { "screen": "no" }, "withhold": "event" }] }"#,
    )
    .unwrap();

    let event = navigate.event().unwrap();

    assert!(rule.hooks(&event));
    assert_eq!(event.field("event"), Some(&json!("CI failed")));
    assert_eq!(navigate.deadline(), DEFAULT_DEADLINE);
    assert_eq!(
        navigate.evidence(),
        [("event".to_owned(), "CI failed".to_owned())]
    );
    assert_eq!(navigate.candidates(), []);

    let recall = Navigate::parse(&json!({
        "protocol": 1, "on": "opencode-history.recall", "candidates": { "p4": "…", "p1": "…" }
    }))
    .unwrap();
    let names: Vec<&str> = recall
        .candidates()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["p1", "p4"], "in name order");
}

#[test]
fn an_evidence_block_named_event_does_not_rename_the_event() {
    let navigate = Navigate::parse(&json!({
        "protocol": 1, "on": "x", "evidence": { "event": "y" }
    }))
    .unwrap();

    let event = navigate.event().unwrap();

    assert_eq!(event.name(), "x");
    assert_eq!(event.evidence(), [("event".to_owned(), "y".to_owned())]);
}

#[test]
fn an_intent_carries_its_rule_and_reads_back() {
    let rule = Rule::parse(
        "sourcefed/triage",
        r#"{ "on": "x", "strategy": [{ "id": "s", "judge": "system-one",
             "ask": { "outcomes": ["yes", "no"], "instructions": "?" } }],
             "then": [{ "if": { "s": "no" }, "withhold": "event" }] }"#,
    )
    .unwrap();

    let intent = Intent::new(rule.id(), &rule.then()[0]);
    let wire = intent.to_json();

    assert_eq!(
        (intent.rule(), intent.name()),
        ("sourcefed/triage", "withhold")
    );
    assert_eq!(intent.payload(), &json!("event"));

    assert_eq!(
        wire,
        json!({ "rule": "sourcefed/triage", "withhold": "event" })
    );
    assert_eq!(Intent::from_json(&wire), Some(intent));
    assert_eq!(Intent::from_json(&json!({ "rule": "r" })), None);
    assert_eq!(
        Intent::from_json(&json!({ "rule": "r", "a": 1, "b": 2 })),
        None
    );
    assert_eq!(Intent::from_json(&json!({ "withhold": "event" })), None);
}

#[test]
fn a_then_named_rule_is_refused() {
    let error = Rule::parse(
        "bad",
        r#"{ "on": "x", "strategy": [{ "id": "s", "judge": "system-one",
             "ask": { "outcomes": ["yes", "no"], "instructions": "?" } }],
             "then": [{ "if": { "s": "no" }, "rule": "event" }] }"#,
    )
    .unwrap_err();

    assert!(error.to_string().contains("cannot name"), "{error}");
}

#[test]
fn a_rule_over_a_limit_is_refused_naming_it() {
    let steps: Vec<Value> = (0..33)
        .map(|index| {
            json!({ "id": format!("s{index}"), "judge": "system-one",
                             "ask": { "outcomes": ["yes", "no"], "instructions": "?" } })
        })
        .collect();
    let text = json!({ "on": "x", "strategy": steps }).to_string();

    let error = Rule::parse("long", &text).unwrap_err();

    assert_eq!(
        Rejection::from(error).to_error()["data"]["limit"],
        json!("steps")
    );
}

#[test]
fn a_run_reports_its_rounds_and_settled_steps_to_three_decimals() {
    let navigate = Navigate::parse(&json!({
        "protocol": 1,
        "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
        "strategy": [
            { "id": "done", "judge": "quick",
              "ask": { "outcomes": ["low", "medium", "high"], "instructions": "How far?" } },
            { "id": "safe", "judge": "quick",
              "ask": { "outcomes": ["yes", "no"], "instructions": "Safe?" } }
        ]
    }))
    .unwrap();
    let Target::Strategy(strategy) = navigate.target() else {
        panic!("an inline strategy")
    };
    let bar = Confidence::Bar(Milli::new(700).unwrap());
    let by = AnsweredBy {
        model: "stand-in".into(),
        placeholder: true,
    };
    let judge = |question: &Question| {
        if question.outcomes().len() == 2 {
            return Outcome::Failed(Failure::Timeout);
        }
        let p = [("low", 0.12), ("medium", 0.45), ("high", 0.43)];
        let p = Probabilities::read(question.outcomes(), p).unwrap();
        Outcome::Value(Answer::settle(question, p, &bar, by.clone()))
    };

    let run = strategy
        .plan()
        .run(&Panel::new().with("quick", &judge))
        .unwrap();
    let events = progress::run_events(Some("r"), &run.events);

    assert_eq!(
        events,
        [
            json!({ "event": "round_ready", "rule": "r", "round": 1, "steps": ["done", "safe"] }),
            json!({ "event": "step_settled", "rule": "r", "step": "done",
                    "answer": { "outcome": "unsure", "p": { "low": 0.12, "medium": 0.45, "high": 0.43 },
                                "level": 1.31, "model": "stand-in", "placeholder": true } }),
            json!({ "event": "step_settled", "rule": "r", "step": "safe", "failed": "timeout" }),
        ]
    );
    assert_eq!(
        progress::verdict(&run.verdict)["safe"],
        json!({ "failed": "timeout" })
    );
    assert_eq!(
        progress::rule_matched("r"),
        json!({ "event": "rule_matched", "rule": "r" })
    );
}

#[test]
fn judges_and_asks_without_a_strategy_explain_what_is_wrong() {
    for fields in [json!({ "judges": {} }), json!({ "asks": {} })] {
        let mut params = json!({ "protocol": 1, "on": "x" });
        params
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());

        let rejected = Navigate::parse(&params).unwrap_err();

        assert!(
            rejected.to_string().contains("come only with an inline"),
            "{rejected}"
        );
    }
}
