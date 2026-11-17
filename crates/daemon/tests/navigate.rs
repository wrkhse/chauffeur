//! `chauffeur.navigate` through the daemon's real socket: intents and
//! verdicts, the events streamed before them, rejections, several calls on
//! one connection, cancellation and admission.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture or a reply missing a field"
)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_daemon::Paths;
use serde_json::{Value, json};

const TRIAGE: &str = r#"{
  "on": "sourcefed.event",
  "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
  "strategy": [{ "id": "screen", "judge": "quick",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "withhold": "event" }]
}"#;

const ANSWERS: &str = r#"{ "answers": [
  { "evidence": { "event": "codecov[bot]: Coverage 81.2%" }, "p": { "yes": 0.1, "no": 0.9 } },
  { "evidence": { "event": "CI failed" }, "p": { "yes": 0.95, "no": 0.05 } },
  { "evidence": { "log": "migrated 42 of 42 tables" }, "p": { "yes": 0.9, "no": 0.1 } }
] }"#;

/// A stand-in for Jev on loopback that answers every question yes, each
/// request after `delay`, as `slow` in a models file whose `stand-in`
/// placeholder reads `answers.json`.
fn models(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().filter_map(Result::ok) {
            thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let _ = reader.read_exact(&mut vec![0; length]);
                thread::sleep(delay);
                let body = r#"{"answers":{"q0":{"type":"noul","noul":0.97}}}"#;
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    json!({
        "models": {
            "stand-in": { "format": "placeholder", "answers": "answers.json" },
            "slow": { "format": "systemone", "endpoint": endpoint, "model": "jev-latest",
                      "key": { "env": "HOME" }, "timeout_ms": 5000 }
        },
        "tiers": { "system-one": ["stand-in"] }
    })
    .to_string()
}

/// Fresh paths with the triage rule as an integration's, and `models`.
///
/// The folder is short and each test's tag keeps it its own: a macOS Unix
/// socket path is refused past 103 bytes, and the temp folder and the
/// socket's own `run/daemon.sock` already take most of that.
fn paths(name: &str, models: &str) -> Paths {
    let root = std::env::temp_dir().join(format!("cn-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("config/rules/sourcefed")).unwrap();
    fs::write(root.join("config/models.json"), models).unwrap();
    fs::write(root.join("config/answers.json"), ANSWERS).unwrap();
    fs::write(root.join("config/rules/sourcefed/triage.json"), TRIAGE).unwrap();
    Paths {
        name: "chauffeur-test".into(),
        socket: root.join("run/daemon.sock"),
        config: root.join("config"),
        distribution: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../distribution"),
        state: root.join("state"),
        models: None,
    }
}

fn start(paths: &Paths) -> UnixStream {
    let running = paths.clone();
    thread::spawn(move || chauffeur_daemon::run(&running));
    let started = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            return stream;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the daemon did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A connection: requests written to it, messages read from it.
struct Caller {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Caller {
    fn new(stream: UnixStream) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self { stream, reader }
    }

    fn navigate(&mut self, id: u64, params: &Value) {
        let request =
            json!({ "jsonrpc": "2.0", "id": id, "method": "chauffeur.navigate", "params": params });
        writeln!(self.stream, "{request}").unwrap();
    }

    fn emit(&mut self, id: u64, params: &Value) {
        let request =
            json!({ "jsonrpc": "2.0", "id": id, "method": "chauffeur.emit", "params": params });
        writeln!(self.stream, "{request}").unwrap();
    }

    fn status(&mut self, id: u64) {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": "chauffeur.status" });
        writeln!(self.stream, "{request}").unwrap();
    }

    fn cancel(&mut self, id: u64) {
        let request =
            json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": id } });
        writeln!(self.stream, "{request}").unwrap();
    }

    fn message(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Messages until the reply to `id`, which is last.
    fn until_reply(&mut self, id: u64) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            let message = self.message();
            let done = message["id"] == id;
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    /// The replies to each of `ids`, in the order they come, skipping the
    /// events before them.
    fn until_replies(&mut self, ids: &[u64]) -> Vec<Value> {
        let mut replies = Vec::new();
        while replies.len() < ids.len() {
            let message = self.message();
            if ids.iter().any(|id| message["id"] == *id) {
                replies.push(message);
            }
        }
        replies
    }
}

fn event(text: &str) -> Value {
    json!({ "protocol": 1, "on": "sourcefed.event", "deadline_ms": 3000,
            "evidence": { "event": text, "monitor": "PR #42", "thread": "feat: navigate" } })
}

/// A test that fills the share with slow runs holds most of the daemon's
/// workers, and the pools are the test binary's: such a test takes its turn,
/// so one starting beside it does not leave either with a worker to admit.
static TURN: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn noise_is_withheld_with_the_events_of_its_run_before_the_intents() {
    let paths = paths("withheld", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &event("codecov[bot]: Coverage 81.2%"));
    let messages = caller.until_reply(1);

    let events: Vec<&Value> = messages.iter().map(|message| &message["params"]).collect();
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[0]["method"], "chauffeur.event");
    assert_eq!(
        *events[0],
        json!({ "id": 1, "event": "rule_matched", "rule": "sourcefed/triage" })
    );
    assert_eq!(events[1]["event"], "round_ready");
    assert_eq!(events[1]["steps"], json!(["screen"]));
    assert_eq!(events[2]["event"], "step_settled");
    assert_eq!(events[2]["answer"]["outcome"], "no");
    assert_eq!(events[2]["answer"]["placeholder"], true);
    assert_eq!(
        messages[3]["result"],
        json!({ "intents": [{ "rule": "sourcefed/triage", "withhold": "event" }] })
    );

    caller.navigate(2, &event("CI failed"));
    let delivered = caller.until_reply(2);
    assert_eq!(
        delivered.last().unwrap()["result"],
        json!({ "intents": [] })
    );
}

#[test]
fn an_event_no_rule_hooks_returns_no_intents_and_no_events() {
    let paths = paths("unhooked", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(
        1,
        &json!({ "protocol": 1, "on": "neo.screen-check", "evidence": {} }),
    );

    assert_eq!(
        caller.until_reply(1),
        [json!({ "jsonrpc": "2.0", "id": 1, "result": { "intents": [] } })]
    );
}

#[test]
fn an_inline_strategy_returns_its_verdict() {
    let paths = paths("inline", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(
        1,
        &json!({
            "protocol": 1,
            "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
            "strategy": [{ "id": "done", "judge": "quick",
                           "ask": { "outcomes": ["yes", "no"], "evidence": ["log"],
                                    "instructions": "Is the migration finished?" } }],
            "evidence": { "log": "migrated 42 of 42 tables" }
        }),
    );
    let messages = caller.until_reply(1);

    let verdict = &messages.last().unwrap()["result"]["verdict"];
    assert_eq!(verdict["done"]["answer"]["outcome"], "yes", "{messages:?}");
    assert_eq!(messages[0]["params"]["event"], "round_ready");
    assert!(messages[0]["params"].get("rule").is_none());
}

#[test]
fn a_request_over_a_limit_or_of_another_protocol_is_rejected_by_reason() {
    let paths = paths("rejected", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &json!({ "protocol": 2, "on": "x" }));
    caller.navigate(
        2,
        &json!({ "protocol": 1, "on": "x", "deadline_ms": 30_001 }),
    );
    // Each call runs on its own thread, so the replies may come in either order.
    let mut replies = [caller.message(), caller.message()];
    replies.sort_by_key(|reply| reply["id"].as_u64());
    let [first, second] = replies;

    assert_eq!(
        first["error"]["data"],
        json!({ "reason": "unsupported_protocol", "accepted": [1] })
    );
    assert_eq!(second["error"]["data"]["limit"], "deadline_ms");
    assert_eq!(second["error"]["code"], -32_602);
}

#[test]
fn an_inline_strategy_with_no_model_for_its_judge_is_unavailable() {
    let paths = paths("unavailable", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(
        1,
        &json!({
            "protocol": 1,
            "strategy": [{ "id": "review", "judge": "system-two",
                           "ask": { "outcomes": ["yes", "no"], "instructions": "Is it safe?" } }]
        }),
    );

    assert_eq!(
        caller.message()["error"]["data"]["reason"],
        "judge_unavailable"
    );
}

/// An inline strategy naming System Two for a project, as a request.
fn system_two(project: &str) -> Value {
    json!({
        "protocol": 1,
        "project": project,
        "strategy": [{ "id": "review", "judge": "system-two",
                       "ask": { "outcomes": ["yes", "no"], "instructions": "Is it safe?" } }]
    })
}

#[test]
fn an_inline_system_two_for_a_project_not_listed_is_unavailable() {
    let paths = paths("s2-unlisted", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &system_two("/nowhere/listed"));
    let reply = caller.message();

    assert_eq!(
        reply["error"]["data"]["reason"], "judge_unavailable",
        "{reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("projects.json"),
        "the refusal names the file that allows a project: {reply}"
    );
}

#[test]
fn an_inline_system_two_for_a_listed_project_is_unavailable_for_its_model() {
    let paths = paths("s2-listed", &models(Duration::ZERO));
    fs::write(paths.config.join("projects.json"), r#"["/work/app"]"#).unwrap();
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &system_two("/work/app"));
    let reply = caller.message();

    assert_eq!(
        reply["error"]["data"]["reason"], "judge_unavailable",
        "{reply}"
    );
    assert!(
        !reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("projects.json"),
        "it passed the allowance and failed for having no model: {reply}"
    );
}

#[test]
fn a_projects_file_naming_a_relative_path_is_refused() {
    let paths = paths("relative", &models(Duration::ZERO));
    fs::write(
        paths.config.join("projects.json"),
        r#"["/work/app", "work/app"]"#,
    )
    .unwrap();

    // Loaded before the socket is bound, so the daemon does not start.
    let refused = chauffeur_daemon::run(&paths).unwrap_err().to_string();

    assert!(refused.contains("projects.json"), "{refused}");
    assert!(refused.contains("work/app"), "{refused}");
}

fn slow_inline(deadline_ms: u64) -> Value {
    json!({
        "protocol": 1, "deadline_ms": deadline_ms,
        "judges": { "careful": { "tier": "system-one", "model": "slow" } },
        "strategy": [{ "id": "done", "judge": "careful",
                       "ask": { "outcomes": ["yes", "no"], "instructions": "Is it finished?" } }]
    })
}

#[test]
fn an_inline_strategy_past_its_deadline_is_rejected_as_deadline() {
    let paths = paths("deadline", &models(Duration::from_secs(2)));
    let mut caller = Caller::new(start(&paths));

    let started = Instant::now();
    caller.navigate(1, &slow_inline(200));

    assert_eq!(caller.message()["error"]["data"]["reason"], "deadline");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn one_connection_carries_several_calls_and_a_fast_one_is_not_held_up() {
    let paths = paths("several", &models(Duration::from_millis(800)));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &slow_inline(3_000));
    caller.navigate(2, &event("CI failed"));

    let first = caller.message();
    assert_eq!(
        first["params"]["id"], 2,
        "the fast call's events come first: {first}"
    );
    let replies: Vec<Value> = caller
        .until_reply(1)
        .into_iter()
        .filter(|m| m.get("id").is_some())
        .collect();
    assert_eq!(replies[0]["id"], 2);
    assert_eq!(
        replies.last().unwrap()["result"]["verdict"]["done"]["answer"]["outcome"],
        "yes"
    );
}

#[test]
fn a_cancelled_request_is_answered_as_cancelled_at_once() {
    let paths = paths("cancel", &models(Duration::from_secs(3)));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &slow_inline(10_000));
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    caller.cancel(1);
    let reply = caller.message();

    assert_eq!(reply["error"]["code"], -32_800, "{reply}");
    assert_eq!(reply["error"]["data"]["reason"], "cancelled");
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// An emit is a call like any other: cancelling it stops its rule and
/// answers the request as cancelled, rather than waiting for the model.
#[test]
fn a_cancelled_emit_is_answered_as_cancelled_at_once() {
    let paths = paths("cancel-emit", &models(Duration::from_secs(3)));
    fs::remove_file(paths.config.join("rules/sourcefed/triage.json")).unwrap();
    fs::write(
        paths.config.join("rules/sourcefed/slow.json"),
        TRIAGE
            .replace("\"confidence\": 0.7", "\"model\": \"slow\"")
            .replace("\"evidence\": [\"event\"]", "\"evidence\": [\"monitor\"]"),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    caller.emit(
        1,
        &json!({ "event": { "event": "sourcefed.event", "monitor": "PR #42" },
                 "deadline_ms": 10_000 }),
    );
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    caller.cancel(1);
    let reply = caller.message();

    assert_eq!(reply["error"]["code"], -32_800, "{reply}");
    assert_eq!(reply["error"]["data"]["reason"], "cancelled");
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// The audit log's runs, oldest first.
fn runs(paths: &Paths) -> Vec<Value> {
    chauffeur_daemon::audit::recent(&paths.state, 256)
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Navigates with ids from `first` until the daemon admits the call, for at
/// most 5 s, and gives the reply it was answered with.
fn admitted(caller: &mut Caller, first: u64, params: &Value) -> Value {
    let started = Instant::now();
    let mut id = first;
    loop {
        caller.navigate(id, params);
        let reply = caller.until_reply(id).pop().unwrap();
        if reply.get("result").is_some() || started.elapsed() > Duration::from_secs(5) {
            return reply;
        }
        id = id.saturating_add(1);
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn closing_two_connections_cancels_their_navigations_and_frees_their_places() {
    let _turn = turn();
    let paths = paths("closed", &models(Duration::from_secs(2)));
    let mut first = Caller::new(start(&paths));
    let mut second = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    for id in 1..=4 {
        first.navigate(id, &slow_inline(10_000));
        second.navigate(id, &slow_inline(10_000));
    }
    thread::sleep(Duration::from_millis(200));

    drop(first);
    drop(second);

    let started = Instant::now();
    while runs(&paths)
        .iter()
        .filter(|run| run["cancelled"] == true)
        .count()
        < 8
    {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            runs(&paths)
        );
        thread::sleep(Duration::from_millis(20));
    }

    // The eight calls are answered, but their workers still ask their
    // models: their places stay taken until those workers return.
    let mut third = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    third.navigate(1, &slow_inline(10_000));
    let busy = third.message();
    assert_eq!(busy["error"]["data"]["reason"], "busy", "{busy}");

    let reply = admitted(&mut third, 2, &event("CI failed"));
    assert!(reply.get("result").is_some(), "{reply}");
}

#[test]
fn cancelling_one_of_four_navigations_frees_its_place_on_the_connection() {
    let _turn = turn();
    let paths = paths("cancel-place", &models(Duration::from_millis(800)));
    let mut caller = Caller::new(start(&paths));
    for id in 1..=4 {
        caller.navigate(id, &slow_inline(10_000));
    }
    thread::sleep(Duration::from_millis(100));

    caller.cancel(1);
    let cancelled = caller.until_reply(1).pop().unwrap();
    assert_eq!(
        cancelled["error"]["data"]["reason"], "cancelled",
        "{cancelled}"
    );

    caller.navigate(5, &slow_inline(10_000));
    let reply = caller.until_reply(5).pop().unwrap();
    assert!(
        reply.get("result").is_some(),
        "the place the cancelled navigation held admits the fifth: {reply}"
    );
}

/// A cancelled navigation answers at once, but its worker keeps its place
/// until it returns from the model: the share stays full meanwhile, and the
/// same connection is answered once the workers are back.
#[test]
fn cancelled_navigations_keep_the_share_until_their_workers_return() {
    let _turn = turn();
    let paths = paths("cooperative", &models(Duration::from_secs(2)));
    let mut first = Caller::new(start(&paths));
    let mut second = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    for id in 1..=4 {
        first.navigate(id, &slow_inline(10_000));
        second.navigate(id, &slow_inline(10_000));
    }
    thread::sleep(Duration::from_millis(100));
    for id in 1..=4 {
        first.cancel(id);
        second.cancel(id);
    }

    let cancelled: Vec<Value> = first
        .until_replies(&[1, 2, 3, 4])
        .into_iter()
        .chain(second.until_replies(&[1, 2, 3, 4]))
        .collect();
    assert!(
        cancelled
            .iter()
            .all(|reply| reply["error"]["data"]["reason"] == "cancelled"),
        "{cancelled:?}"
    );

    let mut third = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    third.navigate(1, &event("CI failed"));
    let busy = third.message();

    assert_eq!(busy["error"]["code"], -32_000, "{busy}");
    assert_eq!(busy["error"]["data"]["reason"], "busy");
    assert_eq!(
        busy["error"]["message"], "the daemon is busy: this caller's share of 8 calls is in use",
        "{busy}"
    );

    let reply = admitted(&mut third, 2, &event("CI failed"));
    assert!(reply.get("result").is_some(), "{reply}");
}

#[test]
fn a_fifth_navigation_on_one_connection_is_busy_at_its_cap_of_four() {
    let paths = paths("busy", &models(Duration::from_secs(1)));
    let mut caller = Caller::new(start(&paths));

    for id in 1..=10 {
        caller.navigate(id, &slow_inline(5_000));
    }
    let mut replies = Vec::new();
    while replies.len() < 10 {
        let message = caller.message();
        if message.get("method").is_none() {
            replies.push(message);
        }
    }

    let busy: Vec<&Value> = replies
        .iter()
        .filter(|reply| reply["error"]["data"]["reason"] == "busy")
        .collect();
    let answered = replies
        .iter()
        .filter(|reply| reply.get("result").is_some())
        .count();
    assert_eq!((busy.len(), answered), (6, 4), "{replies:?}");
    for reply in busy {
        assert_eq!(
            reply["error"]["message"],
            "the daemon is busy: this connection carries 4 navigations already",
            "{replies:?}"
        );
    }
}

/// Two connections take the integration share of eight between them, so a
/// ninth navigation from a third is refused at once, and admitted once the
/// eight slow runs return and give their places back.
#[test]
fn past_the_integration_share_of_eight_a_ninth_navigation_is_busy() {
    let _turn = turn();
    let paths = paths("share", &models(Duration::from_secs(2)));
    let mut first = Caller::new(start(&paths));
    let mut second = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    for id in 1..=4 {
        first.navigate(id, &slow_inline(5_000));
        second.navigate(id, &slow_inline(5_000));
    }
    // A connection handles its lines in order, so each status reply proves
    // the four navigations before it are admitted: the share is full before
    // the third connection asks.
    first.status(5);
    second.status(5);
    first.until_reply(5);
    second.until_reply(5);

    let mut third = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    third.navigate(1, &slow_inline(5_000));
    let refused = third.message();

    assert_eq!(refused["error"]["code"], -32_000, "{refused}");
    assert_eq!(refused["error"]["data"]["reason"], "busy");
    assert_eq!(
        refused["error"]["message"], "the daemon is busy: this caller's share of 8 calls is in use",
        "{refused}"
    );

    let answered: Vec<Value> = first
        .until_replies(&[1, 2, 3, 4])
        .into_iter()
        .chain(second.until_replies(&[1, 2, 3, 4]))
        .collect();
    assert!(
        answered.iter().all(|reply| reply.get("result").is_some()),
        "the eight slow runs return: {answered:?}"
    );

    // A worker gives its place back just after its reply.
    thread::sleep(Duration::from_millis(200));
    third.navigate(2, &slow_inline(5_000));
    let reply = third.until_reply(2).pop().unwrap();
    assert!(reply.get("result").is_some(), "{reply}");
}

#[test]
fn two_matching_rules_both_return_and_a_late_one_drops_only_its_own() {
    let paths = paths("two-rules", &models(Duration::from_secs(2)));
    fs::write(
        paths.config.join("rules/sourcefed/also.json"),
        TRIAGE.replace("\"withhold\": \"event\"", "\"label\": \"noise\""),
    )
    .unwrap();
    fs::write(
        paths.config.join("rules/sourcefed/late.json"),
        TRIAGE
            .replace("\"confidence\": 0.7", "\"model\": \"slow\"")
            .replace("\"no\" }", "\"yes\" }"),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    let started = Instant::now();
    let mut request = event("codecov[bot]: Coverage 81.2%");
    request["deadline_ms"] = json!(500);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/also", "label": "noise" }, { "rule": "sourcefed/triage", "withhold": "event" }])
    );
    let matched = messages
        .iter()
        .filter(|m| m["params"]["event"] == "rule_matched")
        .count();
    assert_eq!(matched, 3);
    assert!(
        runs(&paths)
            .iter()
            .any(|run| run["rule"] == "sourcefed/late" && run["deadline"] == true)
    );
}

/// A deadline that passes before a rule's strategy settles still answers: no
/// step event is sent, and each hooked rule's line says the deadline came
/// first. The deadline is long enough for the dispatcher to start and
/// announce both rules, and short enough that neither settles against the
/// 300 ms stand-in. A `skipped` line takes more hooked rules than the
/// dispatcher has workers, which one request cannot arrange through the
/// socket; the daemon's own test covers that line.
#[test]
fn a_deadline_that_passes_before_a_rule_runs_its_strategy_still_answers() {
    let paths = paths("deadline-rules", &models(Duration::from_millis(300)));
    let slow = TRIAGE.replace("\"confidence\": 0.7", "\"model\": \"slow\"");
    fs::write(paths.config.join("rules/sourcefed/triage.json"), &slow).unwrap();
    fs::write(
        paths.config.join("rules/sourcefed/also.json"),
        slow.replace("\"withhold\": \"event\"", "\"label\": \"noise\""),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    let started = Instant::now();
    let mut request = event("CI failed");
    request["deadline_ms"] = json!(100);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(messages.last().unwrap()["result"], json!({ "intents": [] }));
    let announced = messages
        .iter()
        .filter(|message| message["params"]["event"] == "rule_matched")
        .count();
    assert_eq!(announced, 2, "{messages:?}");
    assert!(
        !messages
            .iter()
            .any(|message| message["params"]["event"] == "round_ready"),
        "no rule runs its strategy: {messages:?}"
    );
    let logged = runs(&paths);
    for rule in ["sourcefed/also", "sourcefed/triage"] {
        assert!(
            logged
                .iter()
                .any(|run| run["rule"] == rule && run["deadline"] == true),
            "{rule} met the deadline: {logged:?}"
        );
    }
}

#[test]
fn a_second_call_with_an_open_id_is_refused_and_the_first_still_answers() {
    let paths = paths("duplicate", &models(Duration::from_millis(500)));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &slow_inline(3_000));
    caller.navigate(1, &slow_inline(3_000));
    let refused = caller.message();
    let answered = caller.until_reply(1);

    assert_eq!(refused["error"]["code"], -32_600, "{refused}");
    assert!(
        answered.last().unwrap().get("result").is_some(),
        "{answered:?}"
    );
}

#[test]
fn a_navigate_sent_as_a_notification_runs_nothing() {
    let paths = paths("notification", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    let request =
        json!({ "jsonrpc": "2.0", "method": "chauffeur.navigate", "params": event("CI failed") });
    writeln!(caller.stream, "{request}").unwrap();
    let status = json!({ "jsonrpc": "2.0", "id": 2, "method": "chauffeur.status" });
    writeln!(caller.stream, "{status}").unwrap();

    assert_eq!(caller.message()["id"], 2);
    assert_eq!(runs(&paths), Vec::<Value>::new());
}

#[test]
fn a_connection_carries_more_calls_one_after_another_than_it_holds_at_once() {
    let paths = paths("many", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    for id in 1..=40 {
        caller.navigate(id, &event("CI failed"));
        let reply = caller.until_reply(id).pop().unwrap();
        assert_eq!(
            reply["result"],
            json!({ "intents": [] }),
            "call {id}: {reply}"
        );
    }
}

#[test]
fn a_call_that_finishes_leaves_the_others_cancellable() {
    let paths = paths("cancel-after", &models(Duration::from_secs(3)));
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &slow_inline(10_000));
    caller.navigate(2, &event("CI failed"));
    caller.until_reply(2);
    let started = Instant::now();
    caller.cancel(1);
    let reply = caller.until_reply(1).pop().unwrap();

    assert_eq!(reply["error"]["data"]["reason"], "cancelled", "{reply}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_line_over_the_limit_is_an_invalid_request_and_closes_the_connection() {
    let paths = paths("oversized", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    // Written from a thread: the daemon stops reading at the limit.
    let mut writer = caller.stream.try_clone().unwrap();
    thread::spawn(move || {
        let _ = writeln!(writer, "{}", "x".repeat(chauffeur_daemon::MAX_LINE + 10));
    });
    let reply = caller.message();

    assert_eq!(reply["error"]["code"], -32_600, "{reply}");
    let mut rest = String::new();
    assert_eq!(
        caller.reader.read_line(&mut rest).unwrap_or(0),
        0,
        "closed: {rest}"
    );
}

#[test]
fn one_client_leaves_navigation_capacity_for_another() {
    let paths = paths("client-share", &models(Duration::from_millis(500)));
    let mut first = Caller::new(start(&paths));
    for id in 1..=5 {
        first.navigate(id, &slow_inline(5_000));
    }
    assert_eq!(first.message()["error"]["data"]["reason"], "busy");

    let mut second = Caller::new(UnixStream::connect(&paths.socket).unwrap());
    second.navigate(1, &event("CI failed"));
    assert!(
        second
            .until_reply(1)
            .last()
            .unwrap()
            .get("result")
            .is_some()
    );
}

#[test]
fn malformed_ids_are_invalid_and_strings_are_preserved() {
    let paths = paths("ids", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));

    for id in [json!(null), json!({}), json!([]), json!(1.5)] {
        writeln!(
            caller.stream,
            "{}",
            json!({ "jsonrpc": "2.0", "id": id, "method": "chauffeur.status" })
        )
        .unwrap();
        assert_eq!(caller.message()["error"]["code"], -32_600);
    }

    writeln!(
        caller.stream,
        "{}",
        json!({ "jsonrpc": "2.0", "id": "s", "method": "chauffeur.status" })
    )
    .unwrap();
    assert_eq!(caller.message()["id"], "s");
}

#[test]
fn an_oversized_line_can_finish_writing_before_its_error_is_read() {
    let paths = paths("whole", &models(Duration::ZERO));
    let mut caller = Caller::new(start(&paths));
    writeln!(
        caller.stream,
        "{}",
        "x".repeat(chauffeur_daemon::MAX_LINE + 32)
    )
    .unwrap();

    assert_eq!(caller.message()["error"]["code"], -32_600);
}
