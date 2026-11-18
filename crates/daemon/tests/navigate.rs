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
use std::path::{Path, PathBuf};
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

/// A project rule hooking the same event, with its own intent.
const PROJECT_RULE: &str = r#"{
  "on": "sourcefed.event",
  "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
  "strategy": [{ "id": "screen", "judge": "quick",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "label": "project-noise" }]
}"#;

/// The same project rule with a cooldown, so a second firing in the same
/// project is suppressed.
const PROJECT_RULE_COOLDOWN: &str = r#"{
  "on": "sourcefed.event",
  "cooldown_seconds": 60,
  "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
  "strategy": [{ "id": "screen", "judge": "quick",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "label": "project-noise" }]
}"#;

/// The same project rule, edited: a longer file, so its signature changes
/// even where a filesystem's modification times are coarse.
const PROJECT_RULE_EDITED: &str = r#"{
  "on": "sourcefed.event",
  "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
  "strategy": [{ "id": "screen", "judge": "quick",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "label": "edited-noise" }]
}"#;

/// A project rule naming a model the user configured: still refused, since
/// a project may not name one.
const PROJECT_MODEL_RULE: &str = r#"{
  "on": "sourcefed.event",
  "judges": { "quick": { "tier": "system-one", "model": "stand-in", "confidence": 0.7 } },
  "strategy": [{ "id": "screen", "judge": "quick",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "label": "project-noise" }]
}"#;

/// A project rule using System Two, which only a listed project may run.
const PROJECT_SYSTEM_TWO: &str = r#"{
  "on": "sourcefed.event",
  "strategy": [{ "id": "review", "judge": "system-two",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Is the agent shown this event?" } }],
  "then": [{ "if": { "review": "yes" }, "label": "approved" }]
}"#;

/// A project rule hooking an event only it matches, using System Two, which
/// only a listed project may run. It reads no configuration when it runs, so
/// a navigation cannot postpone a reload by opening the daemon's files.
const PROJECT_REVIEW: &str = r#"{
  "on": "project.review",
  "strategy": [{ "id": "review", "judge": "system-two",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Is the agent shown this event?" } }],
  "then": [{ "if": { "review": "yes" }, "label": "approved" }]
}"#;

/// A project's own models file: never read, so the System Two placeholder it
/// names cannot answer a project rule.
const PROJECT_PLACEHOLDER: &str = r#"{
  "models": { "own-stand-in": { "format": "placeholder",
                                "otherwise": { "yes": 1, "no": 0 } } },
  "tiers": { "system-two": ["own-stand-in"] }
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

/// Writes `rule` to `name` under `paths`'s project folder's
/// `.chauffeur/rules/`, returning the project folder.
fn project_rule(paths: &Paths, name: &str, rule: &str) -> PathBuf {
    let project = paths.state.parent().unwrap().join("project");
    let path = project.join(".chauffeur/rules").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, rule).unwrap();
    project
}

/// `request`, naming `project`.
fn with_project(mut request: Value, project: &Path) -> Value {
    request["project"] = json!(project.display().to_string());
    request
}

/// The IDs of the rules whose `rule_matched` events `messages` carries,
/// sorted: each run announces on a worker of its own, so the order two rules
/// start in is not fixed.
fn matched(messages: &[Value]) -> Vec<&str> {
    let mut matched: Vec<&str> = messages
        .iter()
        .filter(|message| message["params"]["event"] == "rule_matched")
        .filter_map(|message| message["params"]["rule"].as_str())
        .collect();
    matched.sort_unstable();
    matched
}

/// Whether `messages`' result carries an intent from `rule`.
fn fired(messages: &[Value], rule: &str) -> bool {
    messages
        .last()
        .and_then(|reply| reply["result"]["intents"].as_array())
        .is_some_and(|intents| intents.iter().any(|intent| intent["rule"] == rule))
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
    assert_eq!(
        reply["error"]["message"],
        "System Two runs only for a project listed in projects.json, and /nowhere/listed is not",
        "the refusal names the project and the file that allows one: {reply}"
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
    assert_eq!(
        reply["error"]["message"], "judge `system-two` has no system-two model to ask",
        "it passed the allowance and failed for having no model: {reply}"
    );
}

/// `projects.json` lists a project's real folder, so a link to that folder is
/// one of its spellings: an inline System Two request through the link passes
/// the allowance and fails only for having no model, as the folder itself
/// does. A project that is not listed is still refused, and the refusal names
/// the path its request gave.
#[test]
fn an_inline_system_two_for_a_listed_project_reached_through_a_symlink_is_allowed() {
    let paths = paths("s2-symlink", &models(Duration::ZERO));
    let project = paths.state.parent().unwrap().join("project");
    fs::create_dir_all(&project).unwrap();
    let canonical = fs::canonicalize(&project).unwrap();
    let link = paths.state.parent().unwrap().join("linked");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    fs::write(
        paths.config.join("projects.json"),
        json!([canonical.display().to_string()]).to_string(),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &system_two(&link.display().to_string()));
    let reply = caller.until_reply(1).pop().unwrap();

    assert_eq!(
        reply["error"]["data"]["reason"], "judge_unavailable",
        "{reply}"
    );
    assert!(
        !reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("projects.json"),
        "the link is a spelling of the listed project: {reply}"
    );

    caller.navigate(2, &system_two("/nowhere/listed"));
    let unlisted = caller.until_reply(2).pop().unwrap();

    assert_eq!(
        unlisted["error"]["message"],
        "System Two runs only for a project listed in projects.json, and /nowhere/listed is not",
        "{unlisted}"
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

    let expected = format!(
        "{}: `work/app` is not an absolute path",
        paths.config.join("projects.json").display()
    );
    assert_eq!(refused, expected);
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

/// A navigation by its `on` event is cancelled like any other call: the
/// rule's step settles as cancelled on the wire, before the reply that
/// answers the call as cancelled.
#[test]
fn a_cancelled_navigation_settles_its_step_as_cancelled_before_the_reply() {
    let paths = paths("cancel-step", &models(Duration::from_secs(3)));
    fs::write(
        paths.config.join("rules/sourcefed/triage.json"),
        TRIAGE.replace("\"confidence\": 0.7", "\"model\": \"slow\""),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    caller.navigate(1, &event("CI failed"));
    thread::sleep(Duration::from_millis(100));
    caller.cancel(1);
    let mut messages = caller.until_reply(1);

    let reply = messages.pop().unwrap();
    assert_eq!(reply["error"]["code"], -32_800, "{reply}");
    assert_eq!(
        messages.pop(),
        Some(json!({
            "jsonrpc": "2.0",
            "method": "chauffeur.event",
            "params": { "event": "step_settled", "rule": "sourcefed/triage",
                        "step": "screen", "failed": "cancelled", "id": 1 }
        })),
        "the step settles as cancelled before the reply"
    );
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

/// A deadline that cuts a rule short after it starts still answers: each
/// hooked rule is announced, then settles its step as `failed` for the
/// deadline, and neither runs its strategy. The deadline is long enough for
/// the dispatcher to start and announce both rules, and short enough that
/// neither settles against the 300 ms stand-in. A `skipped` line takes more
/// hooked rules than the dispatcher has workers, which one request cannot
/// arrange through the socket; the daemon's own test covers that line.
#[test]
fn a_deadline_that_cuts_a_rule_short_settles_each_step_as_failed() {
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
    for rule in ["sourcefed/also", "sourcefed/triage"] {
        let announced = messages
            .iter()
            .position(|message| {
                message["params"]["event"] == "rule_matched" && message["params"]["rule"] == rule
            })
            .unwrap_or_else(|| panic!("{rule} was not announced: {messages:?}"));
        let settled = messages
            .iter()
            .position(|message| {
                message["params"]["event"] == "step_settled"
                    && message["params"]["rule"] == rule
                    && message["params"]["step"] == "screen"
                    && message["params"]["failed"] == "deadline"
            })
            .unwrap_or_else(|| panic!("{rule} did not settle its step: {messages:?}"));
        assert!(
            announced < settled,
            "{rule} settled before it was announced: {messages:?}"
        );
    }
    let settled = messages
        .iter()
        .filter(|message| message["params"]["event"] == "step_settled")
        .count();
    assert_eq!(settled, 2, "one step each: {messages:?}");
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
fn a_project_rule_returns_its_intents_alongside_the_integrations() {
    let paths = paths("project-rule", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/project.json", PROJECT_RULE);
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage", "team/project"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([
            { "rule": "sourcefed/triage", "withhold": "event" },
            { "rule": "team/project", "label": "project-noise" }
        ]),
        "{messages:?}"
    );
}

#[test]
fn a_project_rule_naming_a_model_is_refused_and_the_integration_answers() {
    let paths = paths("project-model", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/model.json", PROJECT_MODEL_RULE);
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }]),
        "{messages:?}"
    );
}

#[test]
fn a_project_system_two_rule_for_an_unlisted_project_is_refused() {
    let paths = paths("project-system-two-unlisted", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/review.json", PROJECT_SYSTEM_TWO);
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }]),
        "{messages:?}"
    );
}

#[test]
fn a_project_system_two_rule_loads_for_a_listed_project() {
    let paths = paths("project-system-two-listed", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/review.json", PROJECT_SYSTEM_TWO);
    fs::write(
        paths.config.join("projects.json"),
        json!([project.display().to_string()]).to_string(),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage", "team/review"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }]),
        "the rule loads and then finds no System Two model: {messages:?}"
    );
}

#[test]
fn a_project_rule_with_a_loaded_rule_id_is_skipped() {
    let paths = paths("project-collision", &models(Duration::ZERO));
    let project = project_rule(&paths, "sourcefed/triage.json", PROJECT_RULE);
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }]),
        "the integration rule runs, not the project's clone: {messages:?}"
    );
}

#[test]
fn editing_a_project_rule_changes_the_next_navigation() {
    let paths = paths("project-edit", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/project.json", PROJECT_RULE);
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let before = caller.until_reply(1);
    assert_eq!(
        before.last().unwrap()["result"]["intents"],
        json!([
            { "rule": "sourcefed/triage", "withhold": "event" },
            { "rule": "team/project", "label": "project-noise" }
        ]),
        "{before:?}"
    );

    fs::write(
        project.join(".chauffeur/rules/team/project.json"),
        PROJECT_RULE_EDITED,
    )
    .unwrap();

    caller.navigate(2, &request);
    let after = caller.until_reply(2);

    assert_eq!(
        after.last().unwrap()["result"]["intents"],
        json!([
            { "rule": "sourcefed/triage", "withhold": "event" },
            { "rule": "team/project", "label": "edited-noise" }
        ]),
        "the edited rule ran: {after:?}"
    );
}

#[test]
fn adding_a_project_rule_changes_the_next_navigation() {
    let paths = paths("project-add", &models(Duration::ZERO));
    let project = paths.state.parent().unwrap().join("project");
    let mut caller = Caller::new(start(&paths));
    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);

    caller.navigate(1, &request);
    assert_eq!(matched(&caller.until_reply(1)), ["sourcefed/triage"]);

    project_rule(&paths, "team/project.json", PROJECT_RULE);
    caller.navigate(2, &request);

    assert_eq!(
        matched(&caller.until_reply(2)),
        ["sourcefed/triage", "team/project"],
        "the added rule ran"
    );
}

#[test]
fn deleting_a_project_rule_changes_the_next_navigation() {
    let paths = paths("project-delete", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/project.json", PROJECT_RULE);
    let mut caller = Caller::new(start(&paths));
    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);

    caller.navigate(1, &request);
    assert_eq!(
        matched(&caller.until_reply(1)),
        ["sourcefed/triage", "team/project"]
    );

    fs::remove_file(project.join(".chauffeur/rules/team/project.json")).unwrap();

    caller.navigate(2, &request);
    assert_eq!(
        matched(&caller.until_reply(2)),
        ["sourcefed/triage"],
        "the deleted rule is gone"
    );
}

#[test]
fn renaming_a_project_rule_changes_the_next_navigation() {
    let paths = paths("project-rename", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/project.json", PROJECT_RULE);
    let mut caller = Caller::new(start(&paths));
    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);

    caller.navigate(1, &request);
    assert_eq!(
        matched(&caller.until_reply(1)),
        ["sourcefed/triage", "team/project"]
    );

    fs::rename(
        project.join(".chauffeur/rules/team/project.json"),
        project.join(".chauffeur/rules/team/renamed.json"),
    )
    .unwrap();

    caller.navigate(2, &request);
    assert_eq!(
        matched(&caller.until_reply(2)),
        ["sourcefed/triage", "team/renamed"],
        "the renamed rule's new ID ran"
    );
}

#[test]
fn emptying_projects_json_stops_a_listed_projects_rule() {
    let paths = paths("project-reload", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/review.json", PROJECT_REVIEW);
    fs::write(
        paths.config.join("projects.json"),
        json!([project.display().to_string()]).to_string(),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));
    // A navigation on this event runs the project rule alone and opens no
    // configuration file, so polling does not keep the reload from settling.
    let request = with_project(
        json!({ "protocol": 1, "on": "project.review", "deadline_ms": 3000,
                "evidence": { "event": "CI failed" } }),
        &project,
    );

    caller.navigate(1, &request);
    let listed = caller.until_reply(1);
    assert_eq!(
        matched(&listed),
        ["team/review"],
        "the listed project's rule loads: {listed:?}"
    );

    // The project folder is untouched: only projects.json changes.
    fs::write(paths.config.join("projects.json"), "[]").unwrap();

    let started = Instant::now();
    let mut unlisted = false;
    for id in 2..=120 {
        caller.navigate(id, &request);
        let messages = caller.until_reply(id);
        if !matched(&messages).contains(&"team/review") {
            unlisted = true;
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the reloaded projects.json still lists the project: {messages:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    assert!(unlisted, "the project rule still ran after 10 seconds");
}

#[test]
fn two_projects_rules_with_the_same_id_do_not_share_a_cooldown() {
    let paths = paths("project-cooldown", &models(Duration::ZERO));
    let root = paths.state.parent().unwrap();
    let a = root.join("a");
    let b = root.join("b");
    for project in [&a, &b] {
        let path = project.join(".chauffeur/rules/team/same.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, PROJECT_RULE_COOLDOWN).unwrap();
    }
    let mut caller = Caller::new(start(&paths));
    let request_a = with_project(event("codecov[bot]: Coverage 81.2%"), &a);
    let request_b = with_project(event("codecov[bot]: Coverage 81.2%"), &b);

    caller.navigate(1, &request_a);
    assert!(
        fired(&caller.until_reply(1), "team/same"),
        "the first project's rule fires"
    );

    caller.navigate(2, &request_a);
    assert!(
        !fired(&caller.until_reply(2), "team/same"),
        "it cools down in its own project"
    );

    caller.navigate(3, &request_b);
    assert!(
        fired(&caller.until_reply(3), "team/same"),
        "the other project's rule with the same ID is not suppressed"
    );

    caller.navigate(4, &request_b);
    assert!(
        !fired(&caller.until_reply(4), "team/same"),
        "the other project's rule cools down on its own"
    );
}

/// One project reached by two spellings is one project: a link to the folder
/// runs its rule, the audit entry names the path the filesystem resolves,
/// and a rule with a cooldown fires once across both spellings.
#[test]
fn one_project_reached_through_a_symlink_runs_its_rule_once() {
    let paths = paths("project-symlink", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/project.json", PROJECT_RULE_COOLDOWN);
    let canonical = fs::canonicalize(&project).unwrap();
    let link = paths.state.parent().unwrap().join("linked");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let mut caller = Caller::new(start(&paths));

    let through_link = with_project(event("codecov[bot]: Coverage 81.2%"), &link);
    caller.navigate(1, &through_link);
    let first = caller.until_reply(1);

    assert!(
        fired(&first, "team/project"),
        "the rule runs through the link: {first:?}"
    );
    let run = runs(&paths)
        .into_iter()
        .find(|run| run["rule"] == "team/project")
        .expect("the project rule ran");
    assert_eq!(
        run["project"],
        json!(canonical.display().to_string()),
        "the audit entry names the canonical path: {run}"
    );

    let direct = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(2, &direct);
    let second = caller.until_reply(2);

    assert!(
        !fired(&second, "team/project"),
        "the same project's other spelling is cooling down: {second:?}"
    );
    let cooled = runs(&paths)
        .into_iter()
        .find(|run| run["rule"] == "team/project" && run["cooldown"] == true)
        .expect("the second run was logged as in cooldown");
    assert_eq!(
        cooled["project"],
        json!(canonical.display().to_string()),
        "the cooling run names the canonical path too: {cooled}"
    );
}

#[test]
fn a_project_models_file_has_no_effect() {
    let paths = paths("project-models", &models(Duration::ZERO));
    let project = project_rule(&paths, "team/review.json", PROJECT_SYSTEM_TWO);
    fs::write(project.join(".chauffeur/models.json"), PROJECT_PLACEHOLDER).unwrap();
    fs::write(
        paths.config.join("projects.json"),
        json!([project.display().to_string()]).to_string(),
    )
    .unwrap();
    let mut caller = Caller::new(start(&paths));

    let request = with_project(event("codecov[bot]: Coverage 81.2%"), &project);
    caller.navigate(1, &request);
    let messages = caller.until_reply(1);

    assert_eq!(matched(&messages), ["sourcefed/triage", "team/review"]);
    assert_eq!(
        messages.last().unwrap()["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }]),
        "the project's placeholder did not answer: {messages:?}"
    );
    let run = runs(&paths)
        .into_iter()
        .find(|run| run["rule"] == "team/review")
        .expect("the project rule ran");
    assert_eq!(
        run["steps"],
        json!([{ "step": "review", "failed": "unavailable" }]),
        "the run had no System Two model, not the project's own: {run}"
    );
    assert!(
        !run.to_string().contains("own-stand-in"),
        "the project's placeholder answered: {run}"
    );
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
