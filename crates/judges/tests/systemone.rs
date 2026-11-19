//! The `systemone` format against a loopback HTTP server that replays the
//! shape captured from the live Jev API (jev-1.13.0).
#![expect(
    clippy::unwrap_used,
    reason = "the loopback server's helpers fail the test on a broken fixture"
)]

mod support;

use std::net::TcpListener;
use std::time::Duration;

use chauffeur_core::{Failure, Outcome, Panel, Plan, Step};
use chauffeur_judges::{
    AnsweredBy, Confidence, Evidence, Fallback, Format, KeySource, Models, NamedJudge, Outcomes,
    Question, Settled, SystemOne, SystemOneAsk, SystemOneEntry, Tier,
};
use serde_json::json;
use support::{serve, thousandths};

const LIVE_RESPONSE: &str = r#"{"model":"jev-1.13.0","answers":{"q0":{"type":"noul","noul":0.97}},"usage":{"input_tokens":397,"output_tokens":73}}"#;

fn jev(endpoint: String, timeout_ms: u64) -> SystemOne {
    let entry = SystemOneEntry {
        endpoint,
        model: "jev-latest".into(),
        key: KeySource {
            env: "TYPESAFE_API_KEY".into(),
        },
        timeout_ms: Some(timeout_ms),
        store: None,
        ask: SystemOneAsk::default(),
    };
    SystemOne::new(&entry, "test-key".into())
}

fn commit_question() -> Question {
    Question::noul(
        "Does this commit message break Conventional Commits?",
        ["commit"],
    )
    .unwrap()
}

fn evidence() -> Evidence {
    Evidence::new([("commit", "fixed stuff"), ("unused", "never sent")])
}

#[test]
fn sends_one_noul_request_and_reads_the_live_answer() {
    let (endpoint, server) = serve("200 OK", LIVE_RESPONSE, Duration::ZERO);
    let question = commit_question();

    let replies = jev(endpoint, 5_000).answer_all(&[&question], &evidence());
    let received = server.join().unwrap();

    assert_eq!(received.path, "/v1/systemone");
    assert_eq!(received.authorization, "authorization: Bearer test-key");
    assert_eq!(received.body["model"], "jev-latest");
    assert_eq!(received.body["questions"]["q0"]["type"], "noul");
    assert_eq!(
        received.body["questions"]["q0"]["instructions"],
        "Does this commit message break Conventional Commits?"
    );
    assert_eq!(received.body["state"], "commit:\nfixed stuff\n\n");
    let p = replies.into_iter().next().unwrap().unwrap();
    assert_eq!(
        p.as_slice().iter().map(|p| p.get()).collect::<Vec<_>>(),
        [970, 30]
    );
}

#[test]
fn jev_in_models_json_answers_its_tier_with_its_key_as_a_real_model() {
    let (endpoint, server) = serve("200 OK", LIVE_RESPONSE, Duration::ZERO);
    let folder = std::env::temp_dir().join(format!("chauffeur-systemone-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("jev.json");
    let models = format!(
        r#"{{ "models": {{ "jev": {{ "format": "systemone", "endpoint": "{endpoint}", "model": "jev-latest",
                                      "key": {{ "env": "TYPESAFE_API_KEY" }} }} }},
             "tiers": {{ "system-one": ["jev"] }} }}"#
    );
    std::fs::write(&file, models).unwrap();
    let models = Models::load(&[file]).unwrap();
    let key = |env: &str| (env == "TYPESAFE_API_KEY").then(|| "test-key".to_owned());

    let fallback = Fallback::build(&models, Tier::SystemOne, None, key).unwrap();
    let evidence = evidence();
    let judge = NamedJudge::new(&fallback, &Confidence::Likeliest, &evidence);
    let plan = Plan::new([Step::new("nudge", "quick", commit_question())]).unwrap();
    let run = plan.run(&Panel::new().with("quick", &judge)).unwrap();
    let received = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&folder);

    assert_eq!(received.authorization, "authorization: Bearer test-key");
    let Some(Outcome::Value(answer)) = run.verdict.get("nudge") else {
        panic!("no answer: {:?}", run.verdict)
    };
    assert_eq!(answer.outcome(), &Settled::Outcome("yes".into()));
    assert_eq!(
        answer.by(),
        &AnsweredBy {
            model: "jev".into(),
            placeholder: false
        }
    );
}

#[test]
fn provider_errors_become_failures_that_say_nothing_else() {
    let cases = [
        ("429 Too Many Requests", "{}", Failure::RateLimited),
        (
            "529 Overloaded",
            r#"{"error":"overloaded"}"#,
            Failure::RateLimited,
        ),
        (
            "402 Payment Required",
            r#"{"detail":{"message":"over budget: sk-secret"}}"#,
            Failure::Unavailable,
        ),
        ("500 Internal Server Error", "{}", Failure::Unavailable),
        ("408 Request Timeout", "{}", Failure::Timeout),
        (
            "200 OK",
            r#"{"answers":{"q0":{"type":"score","noul":0.9}}}"#,
            Failure::Invalid,
        ),
        ("200 OK", "not json", Failure::Invalid),
        ("200 OK", r#"{"answers":{}}"#, Failure::Invalid),
        (
            "200 OK",
            r#"{"answers":{"q0":{"type":"noul","noul":1.4}}}"#,
            Failure::Invalid,
        ),
        (
            "200 OK",
            r#"{"answers":{"q0":{"type":"choice","choice":"yes"}}}"#,
            Failure::Invalid,
        ),
        (
            "200 OK",
            r#"{"answers":{"q0":{"type":"noul","noul":0.9,"confidence":1.1}}}"#,
            Failure::Invalid,
        ),
    ];
    for (status, body, expected) in cases {
        let (endpoint, server) = serve(status, body, Duration::ZERO);
        let replies = jev(endpoint, 5_000).answer_all(&[&commit_question()], &evidence());
        server.join().unwrap();
        assert_eq!(replies, [Err(expected)], "{status} {body}");
    }
}

#[test]
fn a_slow_reply_times_out_and_a_closed_port_is_unavailable() {
    let (endpoint, server) = serve("200 OK", LIVE_RESPONSE, Duration::from_millis(600));
    assert_eq!(
        jev(endpoint, 150).answer_all(&[&commit_question()], &evidence()),
        [Err(Failure::Timeout)]
    );
    drop(server);

    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    assert_eq!(
        jev(endpoint, 1_000).answer_all(&[&commit_question()], &evidence()),
        [Err(Failure::Unavailable)]
    );
}

#[test]
fn a_reply_over_the_size_bound_is_invalid() {
    let huge: &'static str = Box::leak(
        format!(r#"{{"answers":{{}},"pad":"{}"}}"#, "x".repeat(300_000)).into_boxed_str(),
    );
    let (endpoint, server) = serve("200 OK", huge, Duration::ZERO);

    let replies = jev(endpoint, 5_000).answer_all(&[&commit_question()], &evidence());
    server.join().unwrap();

    assert_eq!(replies, [Err(Failure::Invalid)]);
}

#[test]
fn a_question_that_is_not_yes_or_no_is_asked_as_a_choice_and_answered() {
    let cases = [
        (
            ["act", "ignore"],
            r#"{"answers":{"q0":{"type":"choice","choice":"act","probabilities":{"act":0.8,"ignore":0.2}}}}"#,
            vec![800, 200],
        ),
        (
            ["yes", "maybe"],
            r#"{"answers":{"q0":{"type":"choice","choice":"maybe","probabilities":{"maybe":0.7,"yes":0.3}}}}"#,
            vec![300, 700],
        ),
    ];

    for (labels, reply, expected) in cases {
        let [first, second] = labels;
        let outcomes = Outcomes::new(labels).unwrap();
        let question = Question::new("What should the agent do?", outcomes, ["commit"]).unwrap();
        let (endpoint, server) = serve("200 OK", reply, Duration::ZERO);

        let replies = jev(endpoint, 5_000).answer_all(&[&question], &evidence());
        let received = server.join().unwrap();

        assert_eq!(
            received.body["questions"],
            json!({ "q0": {
                "type": "choice",
                "instructions": "What should the agent do?",
                "criteria": { first: first, second: second }
            } }),
            "{labels:?}"
        );
        assert_eq!(thousandths(replies), [Ok(expected)], "{labels:?}");
    }
}
