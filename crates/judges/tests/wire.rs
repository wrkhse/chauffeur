//! The `systemone` format's `choice` and `score` questions over loopback
//! HTTP, replaying the replies of `main`'s `judges/{jev,subq,glide}/tests/wire.rs`
//! with a question's ID as its position in the round.
#![expect(
    clippy::unwrap_used,
    reason = "the loopback server's helpers fail the test on a broken fixture"
)]

mod support;

use std::time::{Duration, Instant};

use chauffeur_core::Failure;
use chauffeur_judges::{
    Answer, AnsweredBy, Confidence, Evidence, Fallback, Format, KeySource, Label, Models, Outcomes,
    Probabilities, Question, Settled, SystemOne, SystemOneAsk, SystemOneEntry, Tier,
};
use serde_json::{Value, json};
use support::{serve, thousandths};

/// The reply captured from the live Jev API (jev-1.13.0): a `choice`, a
/// `noul` and a `score`.
const JEV_REPLY: &str = r#"{"model":"jev-1.13.0","answers":{"q0":{"type":"choice","choice":"openai/gpt-6-sol","confidence":0.53,"probabilities":{"openai/gpt-6-sol":0.76,"stay":0.24}},"q1":{"type":"noul","noul":0.97},"q2":{"type":"score","score":1.01,"confidence":0.54,"legend":{"0":"low","1":"medium","2":"high"},"probabilities":{"0":0.15,"1":0.69,"2":0.16}}},"usage":{"input_tokens":397,"output_tokens":73}}"#;

/// The reply of `subq-snap-preview`: a `noul`, a `choice` and a `score`.
const SUBQ_REPLY: &str = r#"{"model":"subq-snap-preview","answers":{"q0":{"type":"noul","noul":0.92},"q1":{"type":"choice","choice":"cli","confidence":0.8,"probabilities":{"cli":0.9,"none":0.1}},"q2":{"type":"score","score":0.75,"confidence":0.5,"legend":{"0":"routine","1":"urgent"},"probabilities":{"0":0.25,"1":0.75}}},"usage":{"input_tokens":100,"output_tokens":0}}"#;

/// The `score` of `fastino/GLiDE`: `score` is the likeliest level's index, and
/// `expected_level` the weighted level its probabilities give.
const GLIDE_REPLY: &str = r#"{"answers":{"q0":{"type":"score","score":0,"expected_level":0.375,"confidence":0.5,"probabilities":{"0":0.625,"1":0.375}}}}"#;

const STATE: &str = "agent hit a limit; leaked [REDACTED] in a log";

/// A model's entry at `endpoint`, with a timeout no test here runs out.
fn entry(endpoint: String, model: &str, store: Option<bool>, ask: SystemOneAsk) -> SystemOneEntry {
    SystemOneEntry {
        endpoint,
        model: model.into(),
        key: KeySource {
            env: "SYSTEM_ONE_KEY".into(),
        },
        timeout_ms: Some(5_000),
        store,
        ask,
    }
}

/// Asks `questions` of `model`, which asks as `ask` and is answered with
/// `reply`. Returns the request it sent and each reply in thousandths.
fn ask(
    (model, store, ask): (&str, Option<bool>, SystemOneAsk),
    reply: &'static str,
    questions: &[&Question],
) -> (Value, Vec<Result<Vec<u16>, Failure>>) {
    let (endpoint, server) = serve("200 OK", reply, Duration::ZERO);
    let client = SystemOne::new(&entry(endpoint, model, store, ask), "test-key".into());

    let replies = client.answer_all(questions, &evidence());
    let received = server.join().unwrap();

    assert_eq!(received.path, "/v1/systemone");
    assert_eq!(received.authorization, "authorization: Bearer test-key");
    (received.body, thousandths(replies))
}

fn evidence() -> Evidence {
    Evidence::new([("log", STATE), ("unused", "never sent")])
}

fn question(instructions: &str, outcomes: impl IntoIterator<Item = impl Into<Label>>) -> Question {
    Question::new(instructions, Outcomes::new(outcomes).unwrap(), ["log"]).unwrap()
}

/// Jev's three questions: which model, whether it is a coding task, and how
/// urgent it is.
fn jev_questions() -> [Question; 3] {
    [
        question(
            "Which model?",
            [
                Label::described("openai/gpt-6-sol", "frontier"),
                Label::described("stay", "do not switch"),
            ],
        ),
        Question::noul("Coding task?", ["log"]).unwrap(),
        question("Urgency?", ["low", "medium", "high"]),
    ]
}

/// A question that is not yes/no.
fn act_or_ignore() -> Question {
    question("What should the agent do?", ["act", "ignore"])
}

/// A reply whose only answer, to `q0`, is `answer`.
fn only(answer: &str) -> &'static str {
    Box::leak(format!(r#"{{"answers":{{"q0":{answer}}}}}"#).into_boxed_str())
}

#[test]
fn jevs_live_reply_answers_a_choice_and_a_noul_in_one_request() {
    let [pick, ok, level] = jev_questions();

    let (request, replies) = ask(
        ("jev-latest", None, SystemOneAsk::Choice),
        JEV_REPLY,
        &[&pick, &ok, &level],
    );

    assert_eq!(
        request,
        json!({
            "model": "jev-latest",
            "state": format!("log:\n{STATE}\n\n"),
            "questions": {
                "q0": {
                    "type": "choice",
                    "instructions": "Which model?",
                    "criteria": { "openai/gpt-6-sol": "frontier", "stay": "do not switch" }
                },
                "q1": { "type": "noul", "instructions": "Coding task?" },
                "q2": {
                    "type": "choice",
                    "instructions": "Urgency?",
                    "criteria": { "low": "low", "medium": "medium", "high": "high" }
                }
            }
        })
    );
    // The reply's `score` answers a question this entry asked as a `choice`,
    // which fails that question and no other.
    assert_eq!(
        replies,
        [Ok(vec![760, 240]), Ok(vec![970, 30]), Err(Failure::Invalid)]
    );
}

#[test]
fn jevs_live_reply_answers_a_score_and_a_noul_in_one_request() {
    let [pick, ok, level] = jev_questions();

    let (request, replies) = ask(
        ("jev-latest", None, SystemOneAsk::Score),
        JEV_REPLY,
        &[&pick, &ok, &level],
    );

    assert_eq!(
        request["questions"],
        json!({
            "q0": {
                "type": "score",
                "instructions": "Which model?",
                "criteria": ["openai/gpt-6-sol", "stay"]
            },
            "q1": { "type": "noul", "instructions": "Coding task?" },
            "q2": {
                "type": "score",
                "instructions": "Urgency?",
                "criteria": ["low", "medium", "high"]
            }
        })
    );
    assert_eq!(
        replies,
        [
            Err(Failure::Invalid),
            Ok(vec![970, 30]),
            Ok(vec![150, 690, 160])
        ]
    );
}

#[test]
fn a_score_settles_to_its_likeliest_level_at_the_level_the_reply_reports() {
    let level = question("Urgency?", ["low", "medium", "high"]);
    let reply = only(
        r#"{"type":"score","score":1.01,"confidence":0.54,"probabilities":{"0":0.15,"1":0.69,"2":0.16}}"#,
    );
    let (endpoint, server) = serve("200 OK", reply, Duration::ZERO);
    let jev = entry(endpoint, "jev-latest", None, SystemOneAsk::Score);

    let p = SystemOne::new(&jev, "test-key".into())
        .answer_all(&[&level], &evidence())
        .remove(0)
        .unwrap();
    server.join().unwrap();
    let by = AnsweredBy {
        model: "jev".into(),
        placeholder: false,
    };
    let answer = Answer::settle(&level, p, &Confidence::Likeliest, by);

    assert_eq!(answer.outcome(), &Settled::Outcome("medium".into()));
    assert!((answer.level() - 1.01).abs() < 1e-9, "{}", answer.level());
}

#[test]
fn subq_is_sent_its_model_and_each_outcomes_description_or_level() {
    let needed = Question::noul("Needed?", ["log"]).unwrap();
    let skill = question(
        "Pick a skill",
        [
            Label::described("cli", "Use CLI"),
            Label::described("none", "No skill"),
        ],
    );
    let urgency = question("Urgency", ["routine", "urgent"]);
    let questions = [&needed, &skill, &urgency];

    let (as_choice, choices) = ask(
        ("subq-snap-preview", None, SystemOneAsk::Choice),
        SUBQ_REPLY,
        &questions,
    );
    let (as_score, scores) = ask(
        ("subq-snap-preview", None, SystemOneAsk::Score),
        SUBQ_REPLY,
        &questions,
    );

    assert_eq!(as_choice["model"], "subq-snap-preview");
    assert_eq!(as_choice["state"], format!("log:\n{STATE}\n\n"));
    assert_eq!(
        as_choice["questions"]["q1"],
        json!({
            "type": "choice",
            "instructions": "Pick a skill",
            "criteria": { "cli": "Use CLI", "none": "No skill" }
        })
    );
    assert_eq!(
        as_score["questions"]["q2"],
        json!({ "type": "score", "instructions": "Urgency", "criteria": ["routine", "urgent"] })
    );
    assert_eq!(
        choices,
        [Ok(vec![920, 80]), Ok(vec![900, 100]), Err(Failure::Invalid)]
    );
    assert_eq!(
        scores,
        [Ok(vec![920, 80]), Err(Failure::Invalid), Ok(vec![250, 750])]
    );
}

#[test]
fn glide_is_sent_store_false_and_its_score_is_read_from_the_probabilities() {
    let check = question("Judge this evidence", ["low", "high"]);

    let (request, replies) = ask(
        ("fastino/GLiDE", Some(false), SystemOneAsk::Score),
        GLIDE_REPLY,
        &[&check],
    );

    assert_eq!(
        request,
        json!({
            "model": "fastino/GLiDE",
            "state": format!("log:\n{STATE}\n\n"),
            "questions": { "q0": {
                "type": "score",
                "instructions": "Judge this evidence",
                "criteria": ["low", "high"]
            } },
            "store": false
        })
    );
    assert_eq!(replies, [Ok(vec![625, 375])]);

    // The level those probabilities give is the reply's `expected_level`.
    let p = Probabilities::read(check.outcomes(), [("low", 0.625), ("high", 0.375)]).unwrap();
    assert!((p.level() - 0.375).abs() < 1e-9, "{}", p.level());
}

#[test]
fn store_is_sent_only_when_the_entry_sets_it() {
    let reply = only(r#"{"type":"noul","noul":0.9,"confidence":0.8}"#);
    let needed = Question::noul("Needed?", ["log"]).unwrap();

    for (store, expected) in [
        (None, None),
        (Some(true), Some(json!(true))),
        (Some(false), Some(json!(false))),
    ] {
        let (request, replies) = ask(("m", store, SystemOneAsk::Choice), reply, &[&needed]);

        assert_eq!(request.get("store"), expected.as_ref(), "{store:?}");
        assert_eq!(replies, [Ok(vec![900, 100])], "{store:?}");
    }
}

#[test]
fn a_score_off_its_scale_or_without_every_levels_probability_is_invalid() {
    let cases = [
        r#"{"type":"score","score":0}"#,
        r#"{"type":"score","score":0,"expected_level":2.0,"probabilities":{"0":0.625,"1":0.375}}"#,
        r#"{"type":"score","score":0,"expected_level":-0.1,"probabilities":{"0":0.625,"1":0.375}}"#,
        r#"{"type":"score","score":0,"expected_level":0.2,"confidence":1.1,"probabilities":{"0":0.8,"1":0.2}}"#,
        r#"{"type":"score","score":2,"probabilities":{"0":0.625,"1":0.375}}"#,
        r#"{"type":"score","score":-1,"probabilities":{"0":0.625,"1":0.375}}"#,
        r#"{"type":"score","probabilities":{"0":1.0}}"#,
        r#"{"type":"score","probabilities":{"0":0.5,"1":0.3,"2":0.2}}"#,
        r#"{"type":"score","probabilities":{"0":0.625,"01":0.375}}"#,
        r#"{"type":"score","probabilities":{"low":0.625,"high":0.375}}"#,
        r#"{"type":"score","probabilities":{"0":1.2,"1":0.375}}"#,
        r#"{"type":"score","probabilities":{"0":0.0,"1":0.0}}"#,
        r#"{"type":"score","probabilities":[0.625,0.375]}"#,
        r#"{"type":"choice","choice":"low","probabilities":{"low":0.625,"high":0.375}}"#,
    ];
    let check = question("Judge this evidence", ["low", "high"]);
    let glide = ("fastino/GLiDE", Some(false), SystemOneAsk::Score);

    for answer in cases {
        let (_, replies) = ask(glide, only(answer), &[&check]);

        assert_eq!(replies, [Err(Failure::Invalid)], "{answer}");
    }
}

#[test]
fn a_score_at_either_end_of_its_scale_is_read() {
    let cases = [
        (
            r#"{"type":"score","score":0,"expected_level":0.0,"probabilities":{"0":1.0,"1":0.0}}"#,
            vec![1000, 0],
        ),
        (
            r#"{"type":"score","score":1,"expected_level":1.0,"probabilities":{"1":1.0,"0":0.0}}"#,
            vec![0, 1000],
        ),
        (
            r#"{"type":"score","probabilities":{"0":0.625,"1":0.375}}"#,
            vec![625, 375],
        ),
    ];
    let check = question("Judge this evidence", ["low", "high"]);
    let glide = ("fastino/GLiDE", Some(false), SystemOneAsk::Score);

    for (answer, expected) in cases {
        let (_, replies) = ask(glide, only(answer), &[&check]);

        assert_eq!(replies, [Ok(expected)], "{answer}");
    }
}

#[test]
fn a_choice_without_every_outcomes_probability_is_invalid() {
    let cases = [
        r#"{"type":"choice","choice":"act"}"#,
        r#"{"type":"choice","choice":"defer","probabilities":{"act":0.8,"ignore":0.2}}"#,
        r#"{"type":"choice","choice":"act","probabilities":{"act":0.8,"ignore":0.1,"defer":0.1}}"#,
        r#"{"type":"choice","choice":"act","probabilities":{"act":1.0}}"#,
        r#"{"type":"choice","choice":"act","probabilities":{"act":1.2,"ignore":0.2}}"#,
        r#"{"type":"choice","choice":"act","probabilities":{"act":0.0,"ignore":0.0}}"#,
        r#"{"type":"choice","choice":"act","probabilities":"act"}"#,
        r#"{"type":"choice","choice":7,"probabilities":{"act":0.8,"ignore":0.2}}"#,
        r#"{"type":"choice","choice":"act","confidence":1.1,"probabilities":{"act":0.8,"ignore":0.2}}"#,
        r#"{"type":"score","score":0.2,"probabilities":{"0":0.8,"1":0.2}}"#,
        r#"{"type":"noul","noul":0.8}"#,
    ];
    let action = act_or_ignore();

    for answer in cases {
        let (_, replies) = ask(("m", None, SystemOneAsk::Choice), only(answer), &[&action]);

        assert_eq!(replies, [Err(Failure::Invalid)], "{answer}");
    }
}

#[test]
fn a_choice_is_read_from_its_probabilities_whatever_it_names() {
    let cases = [
        r#"{"type":"choice","choice":"act","confidence":0.8,"probabilities":{"act":0.8,"ignore":0.2}}"#,
        r#"{"type":"choice","choice":"ignore","probabilities":{"ignore":0.2,"act":0.8}}"#,
        r#"{"type":"choice","probabilities":{"act":0.8,"ignore":0.2}}"#,
    ];
    let action = act_or_ignore();

    for answer in cases {
        let (_, replies) = ask(("m", None, SystemOneAsk::Choice), only(answer), &[&action]);

        assert_eq!(replies, [Ok(vec![800, 200])], "{answer}");
    }
}

#[test]
fn a_noul_outside_zero_to_one_or_without_noul_is_invalid() {
    let cases = [
        r#"{"type":"noul","noul":1.5}"#,
        r#"{"type":"noul","noul":-0.5}"#,
        r#"{"type":"noul"}"#,
    ];
    let needed = Question::noul("Needed?", ["log"]).unwrap();

    for answer in cases {
        let (request, replies) = ask(("m", None, SystemOneAsk::Choice), only(answer), &[&needed]);

        assert_eq!(
            request,
            json!({
                "model": "m",
                "state": format!("log:\n{STATE}\n\n"),
                "questions": { "q0": { "type": "noul", "instructions": "Needed?" } }
            }),
            "{answer}"
        );
        assert_eq!(replies, [Err(Failure::Invalid)], "{answer}");
    }
}

#[test]
fn an_answer_that_cannot_be_read_fails_only_its_question() {
    let reply = r#"{"answers":{
        "q0":{"type":"noul","noul":0.9},
        "q1":{"type":"choice","choice":"act","probabilities":"none"},
        "q2":{"type":"choice","choice":"ignore","probabilities":{"act":0.1,"ignore":0.9}},
        "q4":{"type":"noul","noul":0.5}
    }}"#;
    let needed = Question::noul("Needed?", ["log"]).unwrap();
    let (first, second, unanswered) = (act_or_ignore(), act_or_ignore(), act_or_ignore());

    let (_, replies) = ask(
        ("m", None, SystemOneAsk::Choice),
        reply,
        &[&needed, &first, &second, &unanswered],
    );

    assert_eq!(
        replies,
        [
            Ok(vec![900, 100]),
            Err(Failure::Invalid),
            Ok(vec![100, 900]),
            Err(Failure::Invalid)
        ]
    );
}

#[test]
fn ask_and_store_in_models_json_reach_the_request() {
    let cases = [
        (
            "",
            r#"{"answers":{"q0":{"type":"choice","choice":"low","probabilities":{"low":0.6,"medium":0.3,"high":0.1}}}}"#,
            json!("choice"),
            None,
            vec![600, 300, 100],
        ),
        (
            r#""ask": "score", "store": false,"#,
            r#"{"answers":{"q0":{"type":"score","score":1.5,"probabilities":{"0":0.1,"1":0.3,"2":0.6}}}}"#,
            json!("score"),
            Some(json!(false)),
            vec![100, 300, 600],
        ),
    ];
    let folder = std::env::temp_dir().join(format!("chauffeur-wire-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let level = question("Urgency?", ["low", "medium", "high"]);

    for (fields, reply, kind, store, expected) in cases {
        let (endpoint, server) = serve("200 OK", reply, Duration::ZERO);
        let file = folder.join("models.json");
        let models = format!(
            r#"{{ "models": {{ "m": {{ "format": "systemone", "endpoint": "{endpoint}", {fields}
                                    "model": "m-1", "key": {{ "env": "SYSTEM_ONE_KEY" }} }} }},
                 "tiers": {{ "system-one": ["m"] }} }}"#
        );
        std::fs::write(&file, models).unwrap();
        let models = Models::load(&[file]).unwrap();
        let fallback =
            Fallback::build(&models, Tier::SystemOne, None, |_| Some("test-key".into())).unwrap();

        let replies = fallback.answer_all(&[&level], &evidence());
        let received = server.join().unwrap();

        assert_eq!(received.body["questions"]["q0"]["type"], kind);
        assert_eq!(received.body.get("store"), store.as_ref(), "{kind}");
        let p = replies.into_iter().map(|reply| reply.map(|(p, _)| p));
        assert_eq!(thousandths(p.collect()), [Ok(expected)], "{kind}");
    }

    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn invalid_and_oversized_replies_fail_every_question_and_say_nothing_else() {
    let oversized: &'static str = Box::leak("x".repeat(262_145).into_boxed_str());
    let cases = [
        (
            "402 Payment Required",
            r#"{"secret":"do-not-log-response"}"#,
            Failure::Unavailable,
        ),
        ("200 OK", "not JSON", Failure::Invalid),
        ("200 OK", r#"{"answers":{}}"#, Failure::Invalid),
        ("200 OK", r#"{"answers":[]}"#, Failure::Invalid),
        ("200 OK", oversized, Failure::Invalid),
    ];
    let [pick, ok, level] = jev_questions();

    for (status, reply, expected) in cases {
        let (endpoint, server) = serve(status, reply, Duration::ZERO);
        let jev = entry(endpoint, "jev-latest", None, SystemOneAsk::Choice);

        let replies =
            SystemOne::new(&jev, "test-key".into()).answer_all(&[&pick, &ok, &level], &evidence());
        server.join().unwrap();

        assert_eq!(replies, vec![Err(expected); 3], "{status}");
    }
}

#[test]
fn a_slow_provider_times_out_every_question() {
    let (endpoint, server) = serve("200 OK", SUBQ_REPLY, Duration::from_millis(600));
    let mut subq = entry(endpoint, "subq-snap-preview", None, SystemOneAsk::Choice);
    subq.timeout_ms = Some(150);
    let [pick, ok, level] = jev_questions();
    let started = Instant::now();

    let replies =
        SystemOne::new(&subq, "test-key".into()).answer_all(&[&pick, &ok, &level], &evidence());
    let took = started.elapsed();
    server.join().unwrap();

    assert_eq!(replies, vec![Err(Failure::Timeout); 3]);
    assert!(took < Duration::from_secs(5), "{took:?}");
}
