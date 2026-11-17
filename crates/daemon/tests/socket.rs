//! The daemon through its real socket, with the repository's distribution
//! and a user configuration whose placeholder stands in for Jev.
#![expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "the helpers fail the test on a broken fixture or a reply missing a field"
)]

use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_daemon::{DaemonError, Paths};
use serde_json::{Value, json};

const NUDGING: &str = r#"{ "models": { "stand-in": { "format": "placeholder", "otherwise": { "yes": 0.94, "no": 0.06 } } },
                          "tiers": { "system-one": ["stand-in"] } }"#;

/// Fresh paths in a private folder of their own, with `models` as the
/// user's `models.json`.
fn paths(name: &str, models: &str) -> Paths {
    let root = std::env::temp_dir().join(format!("chauffeur-daemon-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("config/rules")).unwrap();
    fs::write(root.join("config/models.json"), models).unwrap();
    Paths {
        name: "chauffeur-test".into(),
        socket: root.join("run/daemon.sock"),
        config: root.join("config"),
        distribution: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../distribution"),
        state: root.join("state"),
        models: None,
    }
}

/// Starts the daemon on its own thread and waits for its socket.
fn start(paths: &Paths) {
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
}

fn send(socket: &Path, line: &str) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Past the connection limit the daemon replies and closes at once, so the write may fail while the reply is readable.
    if let Err(error) = writeln!(stream, "{line}") {
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ),
            "{error}"
        );
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

/// Reads the reply while writing the oversized line: the daemon may close
/// before the last write. Both directions time out; shutdown releases the
/// writer before joining it, including when reading fails.
fn send_oversized(socket: &Path, line: &str) -> Value {
    let stream = UnixStream::connect(socket).unwrap();
    let limit = Some(Duration::from_secs(5));
    stream.set_read_timeout(limit).unwrap();
    stream.set_write_timeout(limit).unwrap();
    let mut writer = stream.try_clone().unwrap();

    thread::scope(|scope| {
        let writing = scope.spawn(move || writeln!(writer, "{line}"));
        let mut reply = String::new();
        let read = BufReader::new(&stream)
            .take(u64::try_from(chauffeur_daemon::MAX_LINE).unwrap())
            .read_line(&mut reply);
        // Do this before examining either result: even a read timeout must
        // release a writer blocked on a daemon that stopped reading. Its
        // result is ignored: the daemon may already have closed, and
        // platforms disagree on the error (macOS often gives EINVAL).
        let _ = stream.shutdown(Shutdown::Both);
        let written = writing.join();

        assert!(written.is_ok(), "the oversized writer panicked");

        if let Err(error) = written.unwrap() {
            assert!(
                matches!(
                    error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                ),
                "cannot write the oversized line: {error}"
            );
        }

        assert!(
            read.is_ok(),
            "cannot read the oversized-line reply: {read:?}"
        );
        assert!(reply.ends_with('\n'), "the reply has no newline: {reply:?}");
        serde_json::from_str(&reply).unwrap()
    })
}

/// The reply to an oversized line read while it is still being written,
/// with what the writer did once the daemon answered and how long after the
/// reply the writer took to finish. The writer's own thread keeps a slow
/// daemon from blocking the read for longer than its timeout.
fn send_and_finish_writing(socket: &Path, line: &str) -> (Value, io::Result<()>, Duration) {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();

    thread::scope(|scope| {
        let writing = scope.spawn(move || writeln!(writer, "{line}"));
        let mut reply = String::new();
        let read = BufReader::new(&stream)
            .take(u64::try_from(chauffeur_daemon::MAX_LINE).unwrap())
            .read_line(&mut reply);
        let answered = Instant::now();
        let written = writing.join().unwrap();
        let lingered = answered.elapsed();

        assert!(
            read.is_ok(),
            "cannot read the oversized-line reply: {read:?}"
        );
        assert!(reply.ends_with('\n'), "the reply has no newline: {reply:?}");
        (serde_json::from_str(&reply).unwrap(), written, lingered)
    })
}

fn call(socket: &Path, method: &str, params: &Value) -> Value {
    let reply = send(
        socket,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }).to_string(),
    );
    assert_eq!(reply["id"], 7);
    reply["result"].clone()
}

/// The runs in the audit log, oldest first.
fn runs(paths: &Paths) -> Vec<Value> {
    chauffeur_daemon::audit::recent(&paths.state, 256)
        .unwrap()
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// A stand-in for Jev on loopback that answers every question yes, each
/// request after `delay`, as many requests as come. Its models file names the
/// key `HOME`, which every test process has.
fn slow_jev(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().filter_map(Result::ok) {
            thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
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
        "models": { "slow": { "format": "systemone", "endpoint": endpoint, "model": "jev-latest",
                              "key": { "env": "HOME" }, "timeout_ms": 5000 } },
        "tiers": { "system-one": ["slow"] }
    })
    .to_string()
}

/// Waits until `chauffeur.status` meets `ready`, at most 5 seconds.
fn status_when(paths: &Paths, ready: impl Fn(&Value) -> bool) -> Value {
    let started = Instant::now();
    loop {
        let status = call(&paths.socket, "chauffeur.status", &Value::Null);
        if ready(&status) {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the daemon never reached it: {status}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn emit(socket: &Path, session: &str, command: &str) -> Value {
    let event = json!({ "event": "tool.after", "session": session, "tool": "shell", "input": { "command": command } });
    call(socket, "chauffeur.emit", &json!({ "event": event }))["actions"].clone()
}

#[test]
fn a_bad_commit_gets_the_nudge_once_per_cooldown_and_the_log_shows_why() {
    let paths = paths("nudge", NUDGING);
    start(&paths);

    let actions = emit(&paths.socket, "s1", "git commit -m 'fixed stuff'");
    assert_eq!(actions[0]["rule"], "conventional-commits");
    assert_eq!(actions[0]["action"], "message.append");
    assert_eq!(actions[0]["payload"]["delivery"], "now");
    assert_eq!(
        emit(&paths.socket, "s1", "ls -la"),
        json!([]),
        "no rule hooks ls"
    );
    assert_eq!(
        emit(&paths.socket, "s1", "git commit -m 'more stuff'"),
        json!([]),
        "cooling down"
    );
    assert_eq!(
        emit(&paths.socket, "s2", "git commit -m 'more stuff'")[0]["action"],
        "message.append",
        "another session"
    );
    assert_eq!(
        emit(&paths.socket, "s1", "git commit -m 'still more'"),
        json!([]),
        "the first session still cools down after the second fired"
    );

    let runs = runs(&paths);
    let first = &runs[0]["steps"][0];
    assert_eq!(first["step"], "nonconventional");
    assert_eq!(first["answer"]["outcome"], "yes");
    assert_eq!(first["answer"]["model"], "stand-in");
    assert_eq!(first["answer"]["placeholder"], true);
    assert_eq!(runs[1]["cooldown"], true);
    assert!(
        runs[1].get("steps").is_none(),
        "a rule cooling down asks no judge: {}",
        runs[1]
    );
    assert_eq!(runs.len(), 4, "ls ran no rule");
    assert!(
        runs[0]["at"].as_str().unwrap().ends_with('Z'),
        "{}",
        runs[0]
    );
}

#[test]
fn every_rule_an_event_hooks_runs_and_returns_in_the_order_rules_load() {
    let paths = paths("several", NUDGING);
    let shipped =
        fs::read_to_string(paths.distribution.join("rules/conventional-commits.json")).unwrap();
    for id in ["also-commits", "zz-commits"] {
        fs::write(paths.config.join(format!("rules/{id}.json")), &shipped).unwrap();
    }
    start(&paths);

    let actions = emit(&paths.socket, "s1", "git commit -m 'fixed stuff'");

    let rules = call(&paths.socket, "chauffeur.status", &Value::Null)["rules"].clone();
    assert_eq!(rules.as_array().unwrap().len(), 3, "{rules}");
    let returned: Vec<Value> = actions
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["rule"].clone())
        .collect();
    assert_eq!(Value::Array(returned), rules);
    let logged: Vec<Value> = runs(&paths).iter().map(|run| run["rule"].clone()).collect();
    assert_eq!(Value::Array(logged), rules);
}

#[test]
fn with_no_model_to_ask_nothing_is_sent_and_the_log_says_unavailable() {
    let paths = paths("unavailable", r#"{ "tiers": { "system-one": [] } }"#);
    start(&paths);

    assert_eq!(
        emit(&paths.socket, "s1", "git commit -m 'fixed stuff'"),
        json!([])
    );

    assert_eq!(runs(&paths)[0]["steps"][0]["failed"], "unavailable");
}

#[test]
fn an_edited_rule_applies_to_the_next_event_and_a_bad_one_is_refused_with_its_path() {
    let paths = paths("reload", NUDGING);
    start(&paths);
    let shipped =
        fs::read_to_string(paths.distribution.join("rules/conventional-commits.json")).unwrap();
    let edited = shipped.replace("git commit", "jj describe");

    fs::write(paths.config.join("rules/conventional-commits.json"), edited).unwrap();
    fs::create_dir_all(paths.config.join("rules/team")).unwrap();
    fs::write(
        paths.config.join("rules/team/broken.json"),
        r#"{ "on": "x", "strategy": [{ "id": "s", "judge": "nobody", "ask": "a" }] }"#,
    )
    .unwrap();

    let started = Instant::now();
    while emit(&paths.socket, "s1", "jj describe -m 'fixed stuff'") == json!([]) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the edit was not loaded"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let status = call(&paths.socket, "chauffeur.status", &Value::Null);
    assert_eq!(
        emit(&paths.socket, "s9", "git commit -m 'fixed stuff'"),
        json!([]),
        "the user's rule replaced the shipped one"
    );
    let refused = status["refused"][0].as_str().unwrap();
    assert!(
        refused.contains("team/broken.json") && refused.contains("rule `team/broken`"),
        "{refused}"
    );
    assert_eq!(status["rules"], json!(["conventional-commits"]));
}

#[test]
fn files_beside_the_shipped_models_that_are_not_json_are_not_read() {
    let mut paths = paths("notes", NUDGING);
    let shipped = paths.distribution.clone();
    let distribution = paths.config.with_file_name("distribution");
    for file in [
        "models/jev.json",
        "rules/conventional-commits.json",
        "safety/redaction.json",
    ] {
        fs::create_dir_all(distribution.join(file).parent().unwrap()).unwrap();
        fs::copy(shipped.join(file), distribution.join(file)).unwrap();
    }
    fs::write(
        distribution.join("models/README.md"),
        "# Shipped models
",
    )
    .unwrap();
    paths.distribution = distribution;
    start(&paths);

    assert_eq!(
        emit(&paths.socket, "s1", "git commit -m 'fixed stuff'")[0]["action"],
        "message.append"
    );
}

#[test]
fn malformed_and_oversized_requests_fail_safely() {
    let paths = paths("malformed", NUDGING);
    start(&paths);

    assert_eq!(send(&paths.socket, "not json")["error"]["code"], -32_700);
    assert_eq!(
        send(
            &paths.socket,
            r#"{ "id": 1, "method": "chauffeur.status" }"#
        )["error"]["code"],
        -32_600
    );
    let unknown = send(
        &paths.socket,
        r#"{ "jsonrpc": "2.0", "id": 1, "method": "chauffeur.nope" }"#,
    );
    assert_eq!(unknown["error"]["code"], -32_601);
    let no_event = send(
        &paths.socket,
        r#"{ "jsonrpc": "2.0", "id": 1, "method": "chauffeur.emit", "params": {} }"#,
    );
    assert_eq!(no_event["error"]["code"], -32_602);

    let request = r#"{ "jsonrpc": "2.0", "id": 7, "method": "chauffeur.status" }"#;
    let newline = 1;
    let padding = " ".repeat(chauffeur_daemon::MAX_LINE - newline - request.len());
    assert_eq!(
        send(&paths.socket, &format!("{request}{padding}"))["result"]["protocol"],
        1,
        "a line of exactly the limit, with its newline, is read"
    );

    let oversized = "x".repeat(chauffeur_daemon::MAX_LINE.saturating_add(10));
    assert!(
        send_oversized(&paths.socket, &oversized)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("over 1 MiB")
    );
    assert_eq!(
        call(&paths.socket, "chauffeur.status", &Value::Null)["protocol"],
        1,
        "the daemon still answers"
    );
}

#[test]
fn a_line_over_the_limit_that_ends_with_a_newline_is_drained_so_the_writer_finishes() {
    let paths = paths("drained", NUDGING);
    start(&paths);
    let line = "x".repeat(chauffeur_daemon::MAX_LINE.saturating_add(512 * 1024));

    let (reply, written, _) = send_and_finish_writing(&paths.socket, &line);

    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("over 1 MiB"),
        "{reply}"
    );
    assert!(
        written.is_ok(),
        "the drain reads through the newline: {written:?}"
    );
}

#[test]
fn a_line_past_the_drain_limit_is_cut_off_and_the_writer_fails() {
    let paths = paths("cut-off", NUDGING);
    start(&paths);
    let line = "x".repeat(
        chauffeur_daemon::MAX_LINE
            .saturating_add(1)
            .saturating_add(2 * 1024 * 1024),
    );

    let (reply, written, lingered) = send_and_finish_writing(&paths.socket, &line);

    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("over 1 MiB"),
        "{reply}"
    );
    assert!(
        written.is_err(),
        "the drain stops before the line's end: {written:?}"
    );
    // The drain stops at one more MiB and closes at once. A drain that keeps
    // the connection open instead holds the writer until its 500 ms budget
    // ends, so the writer must be released well before that.
    assert!(
        lingered < Duration::from_millis(300),
        "the daemon stopped draining at its limit: {written:?} after {lingered:?}"
    );
    if let Err(error) = written {
        assert!(
            matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ),
            "the writer reaches the closed daemon: {error}"
        );
    }
}

#[test]
fn a_line_that_ends_at_the_limit_is_answered_without_draining() {
    let paths = paths("line-end", NUDGING);
    start(&paths);
    let stream = UnixStream::connect(&paths.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let line = "x".repeat(chauffeur_daemon::MAX_LINE);

    thread::scope(|scope| {
        let writing = scope.spawn(move || writeln!(writer, "{line}"));
        let mut reply = String::new();
        let read = BufReader::new(&stream)
            .take(u64::try_from(chauffeur_daemon::MAX_LINE).unwrap())
            .read_line(&mut reply);
        assert!(read.is_ok(), "cannot read the line's reply: {read:?}");
        assert!(reply.ends_with('\n'), "the reply has no newline: {reply:?}");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"]["code"], -32_600, "{reply}");
        assert!(writing.join().unwrap().is_ok(), "the line is written");

        // The line ended with its newline, so the daemon answers and closes
        // rather than draining on. Writes with no newline then reach the
        // closed daemon instead of being read away.
        let mut stream = &stream;
        let started = Instant::now();
        let mut failed = None;
        while started.elapsed() < Duration::from_millis(400) {
            if let Err(error) = write!(stream, "{}", "x".repeat(4096)) {
                failed = Some(error);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            failed.is_some(),
            "the daemon kept reading after the line ended"
        );
        if let Some(error) = failed {
            assert!(
                matches!(
                    error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                ),
                "the write reaches the closed daemon: {error}"
            );
        }
    });
}

#[test]
fn a_second_daemon_finds_the_first_and_an_open_folder_is_refused() {
    let paths = paths("second", NUDGING);
    start(&paths);

    assert!(matches!(
        chauffeur_daemon::run(&paths),
        Err(DaemonError::Running(_))
    ));

    let open = Paths {
        socket: paths.socket.with_file_name("open/daemon.sock"),
        ..paths
    };
    fs::create_dir_all(open.socket.parent().unwrap()).unwrap();
    fs::set_permissions(
        open.socket.parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let error = chauffeur_daemon::run(&open).unwrap_err();
    assert!(
        error.to_string().ends_with("must have mode 0700"),
        "{error}"
    );
}

#[test]
fn rules_are_read_from_at_most_256_folders() {
    let paths = paths("folders", NUDGING);
    let shipped =
        fs::read_to_string(paths.distribution.join("rules/conventional-commits.json")).unwrap();
    for index in 0..300 {
        let folder = paths.config.join(format!("rules/f{index:03}"));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("r.json"), &shipped).unwrap();
    }
    start(&paths);

    let status = call(&paths.socket, "chauffeur.status", &Value::Null);

    // rules/ itself and 255 below it, besides the shipped rule.
    assert_eq!(
        status["rules"].as_array().unwrap().len(),
        1 + 255,
        "{status}"
    );
}

#[test]
fn past_the_connection_limit_a_caller_is_told_so_and_served_once_one_closes() {
    let paths = paths("busy", NUDGING);
    start(&paths);
    let open: Vec<UnixStream> = (0..chauffeur_daemon::MAX_CONNECTIONS)
        .map(|_| UnixStream::connect(&paths.socket).unwrap())
        .collect();
    // Each is answered once it is served, so all are counted before the next.
    for mut stream in &open {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            stream,
            r#"{{ "jsonrpc": "2.0", "id": 1, "method": "chauffeur.status" }}"#
        )
        .unwrap();
        BufReader::new(stream)
            .read_line(&mut String::new())
            .unwrap();
    }

    let refused = send(
        &paths.socket,
        r#"{ "jsonrpc": "2.0", "id": 7, "method": "chauffeur.status" }"#,
    );
    assert_eq!(refused["error"]["code"], -32_000, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("busy"),
        "{refused}"
    );

    drop(open);
    let started = Instant::now();
    let status = r#"{ "jsonrpc": "2.0", "id": 7, "method": "chauffeur.status" }"#;
    while send(&paths.socket, status).get("result").is_none() {
        assert!(started.elapsed() < Duration::from_secs(5), "still busy");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_rule_edited_into_one_that_does_not_load_keeps_running_as_it_was() {
    let paths = paths("broken-edit", NUDGING);
    let shipped =
        fs::read_to_string(paths.distribution.join("rules/conventional-commits.json")).unwrap();
    let user = paths.config.join("rules/conventional-commits.json");
    fs::write(&user, shipped.replace("git commit", "jj describe")).unwrap();
    start(&paths);
    assert_eq!(
        emit(&paths.socket, "s1", "jj describe -m 'fixed stuff'")[0]["action"],
        "message.append"
    );

    // Saved halfway through an edit.
    fs::write(&user, r#"{ "on": "tool.after", "#).unwrap();
    let status = status_when(&paths, |status| status["refused"] != json!([]));

    assert_eq!(
        emit(&paths.socket, "s2", "jj describe -m 'fixed stuff'")[0]["action"],
        "message.append",
        "the version that loaded before still runs"
    );
    assert_eq!(
        emit(&paths.socket, "s3", "git commit -m 'fixed stuff'"),
        json!([]),
        "the shipped rule does not take its place"
    );
    assert_eq!(status["rules"], json!(["conventional-commits"]));
    let refused = status["refused"][0].as_str().unwrap();
    assert!(
        refused.contains("conventional-commits.json") && refused.contains("still runs"),
        "{refused}"
    );

    fs::remove_file(&user).unwrap();
    status_when(&paths, |status| status["refused"] == json!([]));
    assert_eq!(
        emit(&paths.socket, "s4", "git commit -m 'fixed stuff'")[0]["action"],
        "message.append",
        "a removed file takes its rule with it, and the shipped one is back"
    );
}

#[test]
fn a_rule_folder_that_links_back_to_itself_loads_each_rule_once() {
    let paths = paths("loop", NUDGING);
    let rules = paths.config.join("rules");
    let shipped =
        fs::read_to_string(paths.distribution.join("rules/conventional-commits.json")).unwrap();
    fs::write(rules.join("mine.json"), shipped).unwrap();
    for name in ["a", "b"] {
        std::os::unix::fs::symlink(&rules, rules.join(name)).unwrap();
    }
    start(&paths);

    let status = call(&paths.socket, "chauffeur.status", &Value::Null);

    assert_eq!(status["rules"], json!(["conventional-commits", "mine"]));
}

#[test]
fn events_at_once_in_one_session_return_the_action_once_per_cooldown() {
    let paths = paths("race", &slow_jev(Duration::from_millis(300)));
    start(&paths);

    // All four start before any is joined, so they reach the daemon at once.
    let mut commits = Vec::new();
    for _ in 0..4 {
        let socket = paths.socket.clone();
        commits.push(thread::spawn(move || {
            emit(&socket, "s1", "git commit -m 'fixed stuff'")
        }));
    }
    let actions: usize = commits
        .into_iter()
        .map(|commit| commit.join().unwrap().as_array().unwrap().len())
        .sum();

    assert_eq!(actions, 1, "one nudge, however the events interleave");
    let runs = runs(&paths);
    assert_eq!(runs.len(), 4);
    let returned = runs
        .iter()
        .filter(|run| run.get("returned").is_some())
        .count();
    let cooling = runs.iter().filter(|run| run["cooldown"] == true).count();
    assert_eq!((returned, cooling), (1, 3), "{runs:?}");
}

/// A notification runs nothing, whatever its method: the emit it names
/// writes no run, and the connection holds no call for it.
#[test]
fn a_notification_runs_nothing() {
    let paths = paths("notification", NUDGING);
    start(&paths);
    let event = json!({ "event": "tool.after", "session": "s1", "tool": "shell",
                        "input": { "command": "git commit -m 'fixed stuff'" } });
    let mut stream = UnixStream::connect(&paths.socket).unwrap();
    let limit = Some(Duration::from_secs(5));
    stream.set_read_timeout(limit).unwrap();
    stream.set_write_timeout(limit).unwrap();
    let mut reading = BufReader::new(stream.try_clone().unwrap());

    writeln!(
        stream,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "chauffeur.emit", "params": { "event": event } })
    )
    .unwrap();
    writeln!(
        stream,
        r#"{{ "jsonrpc": "2.0", "id": 1, "method": "chauffeur.status" }}"#
    )
    .unwrap();
    let mut reply = String::new();
    reading.read_line(&mut reply).unwrap();
    assert!(reply.contains("\"result\""), "{reply}");

    // The daemon joins a connection's calls before it closes it, so its EOF
    // is the barrier after which no run can follow.
    stream.shutdown(Shutdown::Write).unwrap();
    let mut ended = String::new();
    let read = reading.read_line(&mut ended).unwrap();

    assert_eq!(read, 0, "the connection ended: {ended:?}");
    assert!(
        runs(&paths).is_empty(),
        "the notification ran a rule: {:?}",
        runs(&paths)
    );
}
