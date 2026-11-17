//! The twelve labelled sourcefed events and v1's recorded gate replies:
//! every event is a `navigate` request, and each reply matches its case.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use chauffeur_protocol::{Navigate, Target};
use serde_json::{Value, json};

fn crate_file(path: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn cases() -> Vec<Value> {
    crate_file("fixtures/sourcefed-events.json")["cases"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn the_fixture_has_twelve_unique_cases_and_an_even_split() {
    let cases = cases();
    assert_eq!(cases.len(), 12);

    let ids: BTreeSet<&str> = cases
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), cases.len(), "every case has its own id");

    let delivered = cases
        .iter()
        .filter(|case| case["expected"]["deliver"].as_bool().unwrap())
        .count();
    let withheld = cases
        .iter()
        .filter(|case| case["expected"]["deliver"].as_bool() == Some(false))
        .count();
    assert_eq!((delivered, withheld), (6, 6));
}

#[test]
fn every_evidence_is_a_navigate_request_for_sourcefed_events() {
    for case in cases() {
        let id = case["id"].as_str().unwrap();
        let request = json!({
            "protocol": 1,
            "on": "sourcefed.event",
            "deadline_ms": 3000,
            "evidence": case["evidence"],
        });

        let navigate = Navigate::parse(&request).unwrap();
        let Target::On(on) = navigate.target() else {
            panic!("{id}: its evidence is not a request for rules");
        };
        assert_eq!(on, "sourcefed.event", "{id}");
    }
}

/// The evidence sourcefed's builder makes for `case` from its `v1.params`,
/// `monitor_source` and optional `subject.title`: the event's five lines, its
/// thread and its source's `describe()`.
fn built_evidence(case: &Value) -> (String, String, String) {
    let params = &case["v1"]["params"];
    let field = |name: &str| params[name].as_str().unwrap();
    let actionable = params["actionable"].as_bool().unwrap();
    let event = [
        format!("source: {}", field("source")),
        format!("kind: {}", field("kind")),
        format!("summary: {}", field("summary")),
        format!("body: {}", field("body")),
        if actionable {
            "actionable: yes".to_owned()
        } else {
            "actionable: no (informational)".to_owned()
        },
    ]
    .join("\n");

    let source = &case["monitor_source"];
    let (label, monitor) = match source["type"].as_str().unwrap() {
        "github" => {
            let repo = source["repo"].as_str().unwrap();
            let pr = source["prNumber"].as_u64().unwrap();
            (format!("#{pr}"), format!("GitHub {repo}#{pr}"))
        }
        "jira" => {
            let issue = source["issueKey"].as_str().unwrap();
            (issue.to_owned(), format!("Jira {issue}"))
        }
        "slack" => {
            let channel = source["channelId"].as_str().unwrap();
            let ts = source.get("threadTs").and_then(Value::as_str);
            ts.map_or_else(
                || ("DM".to_owned(), format!("Slack DM {channel}")),
                |ts| ("thread".to_owned(), format!("Slack {channel} thread {ts}")),
            )
        }
        other => (
            format!("unknown thread for `{other}`"),
            format!("unknown monitor source `{other}`"),
        ),
    };

    let label = label.trim();
    let label = (!label.is_empty()).then_some(label);
    let title = case["subject"]["title"].as_str().map(str::trim);
    let title = title.filter(|title| !title.is_empty());
    let thread = match (label, title) {
        (Some(label), Some(title)) => format!("{label}: {title}"),
        (Some(label), None) => label.to_owned(),
        (None, Some(title)) => title.to_owned(),
        (None, None) => String::new(),
    };

    (event, thread, monitor)
}

#[test]
fn every_evidence_is_what_sourcefeds_builder_makes() {
    for case in cases() {
        let id = case["id"].as_str().unwrap();
        let (event, thread, monitor) = built_evidence(&case);
        let evidence = &case["evidence"];

        assert_eq!(evidence["event"].as_str(), Some(event.as_str()), "{id}");
        assert_eq!(evidence["thread"].as_str(), Some(thread.as_str()), "{id}");
        assert_eq!(evidence["monitor"].as_str(), Some(monitor.as_str()), "{id}");
    }
}

#[test]
fn v1_replies_cover_every_case_and_deliver_as_it_expects() {
    let cases = cases();
    let recorded = crate_file("fixtures/sourcefed-events.v1.json");
    let replies = recorded["replies"].as_object().unwrap();

    let ids: BTreeSet<&str> = cases
        .iter()
        .map(|case| case["id"].as_str().unwrap())
        .collect();
    let v1_ids: BTreeSet<&str> = replies.keys().map(String::as_str).collect();
    assert_eq!(v1_ids, ids, "v1's replies name exactly the fixture's ids");

    for case in &cases {
        let id = case["id"].as_str().unwrap();
        let deliver = case["expected"]["deliver"].as_bool().unwrap();
        let reply = &replies[id];

        assert_eq!(reply["deliver"].as_bool(), Some(deliver), "{id}");
        let gated = reply["reply"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| {
                effect["type"].as_str() == Some("gate")
                    && effect["deliver"].as_bool() == Some(false)
            });
        assert_eq!(deliver, !gated, "{id}");
    }
}
