//! `chauffeur navigate` as a hook script runs it: the request on stdin, the
//! messages on stdout, the exit status. The daemon runs on a thread of this
//! test, on a private socket, with placeholders for every model.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture or output that is not JSON"
)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::mem::DropGuard;
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_daemon::Paths;
use serde_json::{Value, json};

const CHAUFFEUR: &str = env!("CARGO_BIN_EXE_chauffeur");

const PLACEHOLDERS: &str = r#"{ "models": { "stand-in": { "format": "placeholder",
                                                          "otherwise": { "yes": 0.1, "no": 0.9 } } },
                               "tiers": { "system-one": ["stand-in"], "system-two": ["stand-in"] } }"#;

/// The project the inline System Two fixture names, listed so the daemon runs
/// it rather than refusing it, as a user who listed the project would.
const PROJECTS: &str = r#"["/Users/me/src/app"]"#;

const TRIAGE: &str = r#"{
  "on": "sourcefed.event",
  "strategy": [{ "id": "screen", "judge": "system-one",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "withhold": "event" }]
}"#;

/// The triage rule with its one judge on `slow`, the stand-in.
const SLOW_TRIAGE: &str = r#"{
  "on": "sourcefed.event",
  "judges": { "careful": { "tier": "system-one", "model": "slow" } },
  "strategy": [{ "id": "screen", "judge": "careful",
                 "ask": { "outcomes": ["yes", "no"], "evidence": ["event"],
                          "instructions": "Should the agent be shown this event now?" } }],
  "then": [{ "if": { "screen": "no" }, "withhold": "event" }]
}"#;

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// A daemon on a thread, with the triage rule, the fixtures' project and
/// `--models` placeholders.
fn daemon(name: &str) -> Paths {
    let root = std::env::temp_dir().join(format!(
        "chauffeur-cli-navigate-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("config/rules/sourcefed")).unwrap();
    fs::write(root.join("placeholders.json"), PLACEHOLDERS).unwrap();
    fs::write(root.join("config/projects.json"), PROJECTS).unwrap();
    fs::write(root.join("config/rules/sourcefed/triage.json"), TRIAGE).unwrap();
    let paths = Paths {
        name: "chauffeur-test".into(),
        socket: root.join("run/daemon.sock"),
        config: root.join("config"),
        distribution: repo("distribution"),
        state: root.join("state"),
        models: Some(root.join("placeholders.json")),
    };
    let running = paths.clone();
    thread::spawn(move || chauffeur_daemon::run(&running));
    let started = Instant::now();
    while UnixStream::connect(&paths.socket).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the daemon did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    paths
}

/// Runs `chauffeur navigate` against `paths`' socket with `request` on stdin.
fn navigate(paths: &Paths, request: &str) -> Output {
    let mut child = Command::new(CHAUFFEUR)
        .arg("navigate")
        .env("CHAUFFEUR_SOCKET", &paths.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn lines(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn an_event_prints_its_events_then_the_intents_and_exits_0() {
    let paths = daemon("withheld");
    let request = r#"{ "protocol": 1, "deadline_ms": 3000, "on": "sourcefed.event",
                       "evidence": { "event": "thanks, looks good" } }"#;

    let output = navigate(&paths, request);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let messages = lines(&output);
    let events: Vec<&Value> = messages
        .iter()
        .map(|message| &message["params"]["event"])
        .collect();
    assert_eq!(
        events,
        [
            &json!("rule_matched"),
            &json!("round_ready"),
            &json!("step_settled"),
            &Value::Null
        ]
    );
    assert_eq!(
        messages[3]["result"]["intents"],
        json!([{ "rule": "sourcefed/triage", "withhold": "event" }])
    );
}

/// The fixtures every binding replays, each with its `fill` applied: `text`
/// repeated `times`, put at the JSON pointer `at` in its params.
fn fixtures() -> Vec<Value> {
    let text = fs::read_to_string(repo("crates/protocol/fixtures/navigate.json")).unwrap();
    let fixtures: Value = serde_json::from_str(&text).unwrap();
    let mut cases = fixtures["cases"].as_array().unwrap().clone();
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

#[test]
fn every_fixture_is_answered_or_rejected_as_it_expects() {
    let paths = daemon("fixtures");

    let cases = fixtures();
    let source: Value = serde_json::from_str(
        &fs::read_to_string(repo("crates/protocol/fixtures/navigate.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cases.len(), source["cases"].as_array().unwrap().len());
    assert_ne!(cases.len(), 0);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let output = navigate(&paths, &case["params"].to_string());
        let reply = lines(&output).pop().unwrap();
        match case["reject"]["reason"].as_str() {
            None => {
                assert_eq!(output.status.code(), Some(0), "{name}: {reply}");
                let key = if case["expect"] == "on" {
                    "intents"
                } else {
                    "verdict"
                };
                assert!(reply["result"].get(key).is_some(), "{name}: {reply}");
            }
            Some(reason) => {
                let code = if reason == "unsupported_protocol" {
                    2
                } else {
                    1
                };
                assert_eq!(output.status.code(), Some(code), "{name}: {reply}");
                assert_eq!(reply["error"]["data"]["reason"], reason, "{name}");
                assert_eq!(
                    reply["error"]["data"].get("limit"),
                    case["reject"].get("limit"),
                    "{name}"
                );
            }
        }
    }
}

#[test]
#[ignore = "starts subprocesses; run only in CI or a disposable container"]
fn closed_pipe_after_first_line_exits_successfully() {
    use std::os::unix::net::UnixListener;

    let root = std::env::temp_dir().join(format!("chauffeur-pipe-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let socket = root.join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let line = format!("{{\"event\":\"{}\"}}\n", "x".repeat(4096));
        for _ in 0..2048 {
            if stream.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
    });
    let output = Command::new("bash")
        .args([
            "-o",
            "pipefail",
            "-c",
            "\"$CHAUFFEUR\" navigate <<<'{}' | head -1 >/dev/null",
        ])
        .env("CHAUFFEUR", CHAUFFEUR)
        .env("CHAUFFEUR_SOCKET", &socket)
        .output()
        .unwrap();
    server.join().unwrap();
    fs::remove_dir_all(root).unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn with_no_daemon_it_exits_2_and_says_so() {
    let missing = std::env::temp_dir().join(format!(
        "chauffeur-cli-none-{}/daemon.sock",
        std::process::id()
    ));
    let paths = Paths {
        socket: missing,
        ..Paths::from_env("chauffeur-test", None)
    };

    let output = navigate(&paths, r#"{ "protocol": 1, "on": "x" }"#);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no daemon answers"));
}

#[test]
fn a_request_that_is_not_json_exits_1() {
    let paths = daemon("not-json");

    let output = navigate(&paths, "{ protocol: 1");

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not JSON"));
}

/// Kills the daemon a test started, however the test ends.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "starts and kills a daemon; run only in CI or a disposable container"]
fn the_daemon_answers_from_the_models_file_it_is_given() {
    let root = std::env::temp_dir().join(format!("chauffeur-cli-models-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("config/chauffeur/rules/sourcefed")).unwrap();
    fs::write(root.join("placeholders.json"), PLACEHOLDERS).unwrap();
    fs::write(
        root.join("config/chauffeur/rules/sourcefed/triage.json"),
        TRIAGE,
    )
    .unwrap();
    let paths = Paths {
        socket: root.join("run/daemon.sock"),
        ..Paths::from_env("chauffeur-test", None)
    };
    let _daemon = Daemon(
        Command::new(CHAUFFEUR)
            .args(["daemon", "run", "--models"])
            .arg(root.join("placeholders.json"))
            .env("CHAUFFEUR_SOCKET", &paths.socket)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("CHAUFFEUR_DISTRIBUTION", repo("distribution"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while UnixStream::connect(&paths.socket).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the daemon did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let output = navigate(
        &paths,
        r#"{ "protocol": 1, "on": "sourcefed.event", "evidence": { "event": "ok" } }"#,
    );

    let messages = lines(&output);
    let settled = &messages[2]["params"]["answer"];
    assert_eq!(
        (settled["model"].as_str(), settled["placeholder"].as_bool()),
        (Some("stand-in"), Some(true))
    );
}

/// A stand-in for Jev on loopback that answers every question `delay` after
/// it is asked. Each request is announced on the returned channel as it
/// arrives, so a test knows the judge is being asked before it answers.
fn slow_judge(delay: Duration) -> (String, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (called, judge_called) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().filter_map(Result::ok) {
            let called = called.clone();
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
                let _ = called.send(());
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
    (endpoint, judge_called)
}

/// A models file whose `slow` model is the stand-in at `endpoint`, in the
/// `system-one` tier.
fn slow_models(endpoint: &str) -> String {
    json!({
        "models": {
            "slow": { "format": "systemone", "endpoint": endpoint, "model": "jev-latest",
                      "key": { "env": "HOME" }, "timeout_ms": 5000 }
        },
        "tiers": { "system-one": ["slow"] }
    })
    .to_string()
}

/// The audit log's runs, oldest first, from the folder the daemon keeps its
/// state in: `$XDG_STATE_HOME` followed by the executable's name.
fn runs(state: &Path) -> Vec<Value> {
    chauffeur_daemon::audit::recent(state, 256)
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A daemon under `root` reading `models`, waited for until it answers its
/// socket. It is killed when its guard drops.
fn serve(root: &Path, models: &Path) -> (Paths, Daemon) {
    let paths = Paths {
        socket: root.join("run/daemon.sock"),
        ..Paths::from_env("chauffeur-test", None)
    };
    let daemon = Daemon(
        Command::new(CHAUFFEUR)
            .args(["daemon", "run", "--models"])
            .arg(models)
            .env("CHAUFFEUR_SOCKET", &paths.socket)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("CHAUFFEUR_DISTRIBUTION", repo("distribution"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while UnixStream::connect(&paths.socket).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the daemon did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    (paths, daemon)
}

/// Kills `chauffeur navigate` while the daemon is waiting on a slow judge:
/// the run is recorded as cancelled, and the next call is answered rather
/// than refused busy.
#[test]
#[ignore = "starts and kills a daemon and a client; run only in CI or a disposable container"]
fn killing_chauffeur_navigate_cancels_its_run_and_frees_its_place() {
    let root = std::env::temp_dir().join(format!("chauffeur-cli-kill-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("config/chauffeur/rules/sourcefed")).unwrap();
    let (endpoint, judge_called) = slow_judge(Duration::from_millis(1500));
    fs::write(root.join("slow.json"), slow_models(&endpoint)).unwrap();
    fs::write(
        root.join("config/chauffeur/rules/sourcefed/triage.json"),
        SLOW_TRIAGE,
    )
    .unwrap();
    let (paths, daemon) = serve(&root, &root.join("slow.json"));

    let request = r#"{ "protocol": 1, "deadline_ms": 10000, "on": "sourcefed.event",
                       "evidence": { "event": "CI failed" } }"#;
    let mut client = DropGuard::new(
        Command::new(CHAUFFEUR)
            .arg("navigate")
            .env("CHAUFFEUR_SOCKET", &paths.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        |mut client| {
            let _ = client.kill();
            let _ = client.wait();
        },
    );
    client
        .stdin
        .take()
        .unwrap()
        .write_all(request.as_bytes())
        .unwrap();
    assert!(
        judge_called.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the judge was never asked"
    );
    let _ = client.kill();
    let _ = client.wait();

    // The daemon keeps its state under the executable's name, `chauffeur`.
    let state = root.join("state/chauffeur");
    let started = Instant::now();
    while !runs(&state)
        .iter()
        .any(|run| run["rule"] == "sourcefed/triage" && run["cancelled"] == true)
    {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            runs(&state)
        );
        thread::sleep(Duration::from_millis(20));
    }

    // The stand-in answers yes, so this rule's `then` returns nothing; what
    // matters is that the call was admitted and answered instead of refused.
    let output = navigate(&paths, request);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let reply = lines(&output).pop().unwrap();
    assert!(reply["result"]["intents"].is_array(), "{reply}");

    drop(daemon);
    let _ = fs::remove_dir_all(root);
}
