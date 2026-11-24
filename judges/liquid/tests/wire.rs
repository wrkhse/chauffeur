//! Contract test against a local HTTP fixture that replays the response shape
//! from Liquid's Decisions API.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chauffeur_core::{AnswerValue, ChoiceOption, Facts, Question, QuestionKind, SystemOne};
use chauffeur_judge_liquid::{DEFAULT_MODEL, LiquidClient, LiquidConfig};
use serde_json::{Value, json};

fn fixture(status: &str, reply: Value) -> (LiquidClient, JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    let status = status.to_string();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("request line");
        assert_eq!(line.trim(), "POST /v1/systemone HTTP/1.1");

        let mut length = 0;
        let mut authorized = false;
        loop {
            line.clear();
            reader.read_line(&mut line).expect("header");
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().expect("length");
                }
                if key.eq_ignore_ascii_case("authorization") {
                    authorized = value.trim() == "Bearer fake-liquid-key";
                }
            }
        }
        assert!(authorized);

        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("body");
        let reply = reply.to_string();
        write!(
            reader.get_mut(),
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .expect("respond");
        serde_json::from_slice(&body).expect("json body")
    });
    let client = LiquidClient::new(LiquidConfig {
        base_url,
        api_key: "fake-liquid-key".into(),
        model: DEFAULT_MODEL.into(),
        timeout: Duration::from_secs(3),
        record: None,
    })
    .expect("client");
    (client, server)
}

fn question(id: &str, kind: QuestionKind) -> Question {
    Question {
        id: id.into(),
        instructions: "Judge this evidence".into(),
        kind,
        facts: Facts::new(),
        second_opinion: Default::default(),
    }
}

fn choice_question() -> Question {
    question(
        "model-router/pick",
        QuestionKind::Choice {
            options: vec![
                ChoiceOption {
                    value: "switch".into(),
                    description: "use the other model".into(),
                },
                ChoiceOption {
                    value: "stay".into(),
                    description: "keep the current model".into(),
                },
            ],
        },
    )
}

#[test]
fn sends_the_default_model_and_parses_answers() {
    let (mut client, server) = fixture(
        "200 OK",
        json!({
            "model": "d1.2-ultra",
            "answers": {
                "skill-exposure/twitter-cli": {"type": "noul", "noul": 0.92},
                "model-router/pick": {"type": "choice", "choice": "stay", "confidence": 0.81}
            }
        }),
    );
    let questions = vec![
        question("skill-exposure/twitter-cli", QuestionKind::Noul),
        choice_question(),
    ];

    let mut answers = client.ask("redacted state", &questions).expect("answers");
    answers.sort_by(|a, b| a.id.cmp(&b.id));
    let request = server.join().expect("fixture");

    assert_eq!(client.name(), "liquid");
    assert_eq!(request["model"], DEFAULT_MODEL);
    assert_eq!(request["state"], "redacted state");
    assert_eq!(
        request["questions"]["skill-exposure/twitter-cli"]["type"],
        "noul"
    );
    assert_eq!(
        request["questions"]["model-router/pick"]["criteria"]["stay"],
        "keep the current model"
    );
    assert!(request.get("store").is_none());
    assert_eq!(answers[0].value, AnswerValue::Choice("stay".into()));
    assert_eq!(answers[0].confidence, Some(0.81));
    assert_eq!(answers[1].value, AnswerValue::Noul(0.92));
}

#[test]
fn http_errors_name_the_provider_without_echoing_the_body() {
    let (mut client, server) = fixture(
        "503 Service Unavailable",
        json!({"detail": "down: sk-secret"}),
    );

    let error = client
        .ask("state", &[question("rules/check", QuestionKind::Noul)])
        .expect_err("HTTP error");

    assert_eq!(error.0, "liquid returned HTTP 503 Service Unavailable");
    assert!(!error.0.contains("sk-secret"));
    server.join().expect("fixture");
}
