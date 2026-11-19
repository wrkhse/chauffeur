#![expect(
    clippy::unwrap_used,
    reason = "the helpers fail the test on a broken fixture"
)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chauffeur_core::{Failure, Outcome};
use chauffeur_judges::Models;
use chauffeur_protocol::{Event, Rule, Strategy};
use chauffeur_protocol::{Navigate, Target};
use chauffeur_runtime::{
    Admission, Cancel, Capacity, Context, Ran, Redactor, Source, run, run_strategy,
};

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

fn commit_rule() -> Arc<Rule> {
    let text = fs::read_to_string(repo("distribution/rules/conventional-commits.json")).unwrap();
    Arc::new(Rule::parse("conventional-commits", &text).unwrap())
}

fn commit(message: &str) -> Event {
    let event = serde_json::json!({
        "event": "tool.after", "session": "s1", "tool": "shell",
        "input": { "command": format!("git commit -m '{message}'") }
    });
    Event::parse(&event.to_string()).unwrap()
}

/// A context whose models come from `models` written to a fresh folder, with
/// the key `test-key` for every model.
fn context(name: &str, models: &str, files: &[(&str, &str)]) -> Arc<Context> {
    let folder =
        std::env::temp_dir().join(format!("chauffeur-runtime-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(&folder).unwrap();
    for (file, text) in files {
        fs::write(folder.join(file), text).unwrap();
    }
    fs::write(folder.join("models.json"), models).unwrap();
    Arc::new(Context {
        models: Models::load(&[folder.join("models.json")]).unwrap(),
        redactor: Redactor::load(&[repo("distribution/safety/redaction.json")]).unwrap(),
        keys: Box::new(|_| Some("test-key".to_owned())),
    })
}

/// `name` keeps each test's folder apart: `cargo test` runs them as threads
/// of one process.
fn placeholder(name: &str, answers: &str) -> Arc<Context> {
    context(
        name,
        r#"{ "models": { "stand-in": { "format": "placeholder", "answers": "answers.json" } },
             "tiers": { "system-one": ["stand-in"] } }"#,
        &[("answers.json", answers)],
    )
}

#[test]
fn a_judge_sees_only_redacted_evidence() {
    let leaked = "fixed AKIAABCDEFGHIJKLMNOP";
    let redacted = "tool: shell\ncommand: git commit -m 'fixed [REDACTED]'\n";
    let answers = format!(
        r#"{{ "answers": [{{ "evidence": {{ "tool_call": {redacted:?} }}, "p": {{ "yes": 0.94, "no": 0.06 }} }}] }}"#
    );

    let rule = commit_rule();
    let ran = run(
        &placeholder("redacted", &answers),
        &rule,
        &commit(leaked),
        Duration::from_secs(2),
    );
    let then = ran.then(&rule).cloned();
    let Ran::Settled(ran) = ran else {
        panic!("the rule did not settle")
    };

    let Some(Outcome::Value(answer)) = ran.verdict.get("nonconventional") else {
        panic!("no answer")
    };
    assert!(answer.by().placeholder);
    assert_eq!(
        then.map(|then| then.name().to_owned()).as_deref(),
        Some("message.append")
    );
}

#[test]
fn with_no_model_answering_the_step_fails_and_nothing_is_returned() {
    let rule = commit_rule();
    let ran = run(
        &placeholder("unanswered", r#"{ "answers": [] }"#),
        &rule,
        &commit("fixed stuff"),
        Duration::from_secs(2),
    );
    let then = ran.then(&rule);
    let Ran::Settled(ran) = &ran else {
        panic!("the rule did not settle")
    };

    assert_eq!(
        ran.verdict.get("nonconventional"),
        Some(&Outcome::Failed(Failure::Unavailable))
    );
    assert!(then.is_none());
}

#[test]
fn a_run_whose_worker_stops_ends_at_once_not_late() {
    let models = r#"{ "models": { "jev": { "format": "systemone", "endpoint": "http://127.0.0.1:9", "model": "jev-latest",
                                          "key": { "env": "TYPESAFE_API_KEY" } } },
                      "tiers": { "system-one": ["jev"] } }"#;
    let context = Arc::into_inner(context("stopped", models, &[])).unwrap();
    let context = Arc::new(Context {
        keys: Box::new(|_| panic!("the worker stops while it builds the judges")),
        ..context
    });

    let started = Instant::now();
    let ran = run(
        &context,
        &commit_rule(),
        &commit("fixed stuff"),
        Duration::from_secs(5),
    );

    assert!(matches!(&ran, Ran::Stopped), "{ran:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_judge_slower_than_the_end_fails_as_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut stream = accept(&listener, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let sent = request(&mut stream);
        thread::sleep(Duration::from_millis(800));
        let reply = r#"{"answers":{"q0":{"type":"noul","noul":0.9}}}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{reply}",
            reply.len()
        );
        sent
    });
    let context = context("deadline", &http_models(&endpoint, 2_000), &[]);
    let started = Instant::now();

    let ran = run(
        &context,
        &commit_rule(),
        &commit("fixed AKIAABCDEFGHIJKLMNOP"),
        Duration::from_millis(1_200),
    );
    let elapsed = started.elapsed();
    let sent = server.join().unwrap();

    assert_deadline(ran);
    assert!(
        (Duration::from_millis(100)..Duration::from_millis(750)).contains(&elapsed),
        "{elapsed:?}"
    );
    assert_eq!(
        sent,
        serde_json::json!({
            "model": "jev-latest",
            "questions": {
                "q0": {
                    "type": "noul",
                    "instructions":
                        "Does this call create a git commit whose message does not follow Conventional Commits?"
                }
            },
            "state": "tool_call:\ntool: shell\ncommand: git commit -m 'fixed [REDACTED]'\n\n\n"
        })
    );
}

#[test]
fn a_budget_inside_the_reserve_settles_without_an_http_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let context = context("short-deadline", &http_models(&endpoint, 2_000), &[]);
    let started = Instant::now();

    let ran = run(
        &context,
        &commit_rule(),
        &commit("fixed stuff"),
        Duration::from_millis(200),
    );

    assert_deadline(ran);
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[expect(clippy::panic, reason = "a missing settled result fails the test")]
fn assert_deadline(ran: Ran) {
    let Ran::Settled(run) = ran else {
        panic!("expected a settled deadline failure, got {ran:?}");
    };
    let failed = Outcome::Failed(Failure::Deadline);

    assert_eq!(
        run.verdict.iter().collect::<Vec<_>>(),
        [("nonconventional", &failed)]
    );
    assert_eq!(
        run.verdict.skipped().collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
}

fn http_models(endpoint: &str, timeout_ms: u64) -> String {
    format!(
        r#"{{ "models": {{ "jev": {{ "format": "systemone", "endpoint": "{endpoint}", "model": "jev-latest",
                                  "key": {{ "env": "TYPESAFE_API_KEY" }}, "timeout_ms": {timeout_ms} }} }},
             "tiers": {{ "system-one": ["jev"] }} }}"#
    )
}

/// Accepts at most one connection, with a bounded listener lifetime.
#[expect(clippy::panic, reason = "a broken loopback listener fails the test")]
fn accept(listener: &TcpListener, wait: Duration) -> Option<std::net::TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();

    while started.elapsed() < wait {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();

                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("the listener failed: {error}"),
        }
    }

    None
}

#[expect(clippy::panic, reason = "an oversized fixture request fails the test")]
fn request(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0;

    for _ in 0..64 {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);

        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if line.trim().is_empty() {
            assert!(length <= 65_536);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();

            return serde_json::from_slice(&body).unwrap();
        }
    }

    panic!("too many request headers");
}

/// A stand-in for Jev on loopback that answers once, after `delay`.
fn slow_jev(delay: Duration) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        if let Some(stream) = accept(&listener, Duration::from_secs(1)) {
            thread::sleep(delay);
            drop(stream);
        }
    });

    (http_models(&endpoint, 5_000), server)
}

fn inline(evidence: &str) -> (Arc<chauffeur_protocol::Strategy>, Vec<(String, String)>) {
    let navigate = Navigate::parse(&serde_json::json!({
        "protocol": 1,
        "judges": { "quick": { "tier": "system-one", "confidence": 0.7 } },
        "strategy": [{ "id": "done", "judge": "quick",
                       "ask": { "outcomes": ["yes", "no"], "evidence": ["log"],
                                "instructions": "Is the migration finished?" } }],
        "evidence": { "log": evidence }
    }))
    .unwrap();
    let evidence = navigate.evidence().to_vec();
    let strategy = match navigate.into_target() {
        Target::Strategy(strategy) => Some(strategy),
        Target::On(_) => None,
    };
    let strategy = strategy.unwrap();
    (Arc::new(strategy), evidence)
}

fn in_ms(milliseconds: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap()
}

/// Fills the integration pool: each run cancels itself inside `keys` and then
/// waits for the test's release, so its worker stays busy after
/// `run_strategy` returns. The first eight are admitted; admission refuses
/// the rest. Returns how many were admitted and how many refused.
fn hold_integration_workers(
    admission: &Admission,
    slow: &Arc<Context>,
    strategy: &Arc<Strategy>,
    evidence: &[(String, String)],
    active: &Mutex<Option<Cancel>>,
) -> (usize, usize) {
    let (mut admitted, mut busy) = (0_usize, 0_usize);
    for _ in 0..32 {
        let cancel = admission.admit(Source::Integration).map_or_else(
            |_| {
                busy = busy.saturating_add(1);
                Cancel::new(Source::Integration)
            },
            |permit| {
                admitted = admitted.saturating_add(1);
                Cancel::new(Source::Integration).admitted(permit)
            },
        );
        *active.lock().unwrap() = Some(cancel.clone());
        let ran = run_strategy(slow, strategy, evidence, in_ms(5_000), &cancel);
        assert!(matches!(ran, Ran::Cancelled), "{ran:?}");
    }
    (admitted, busy)
}

#[test]
fn an_inline_strategy_settles_over_the_requests_evidence() {
    let answers = r#"{ "answers": [{ "evidence": { "log": "migrated 42 of 42 tables" },
                                     "p": { "yes": 0.9, "no": 0.1 } }] }"#;
    let (strategy, evidence) = inline("migrated 42 of 42 tables");

    let ran = run_strategy(
        &placeholder("inline", answers),
        &strategy,
        &evidence,
        in_ms(2_000),
        &Cancel::new(Source::Harness),
    );

    let Ran::Settled(ran) = ran else {
        panic!("the strategy did not settle: {ran:?}")
    };
    let Some(Outcome::Value(answer)) = ran.verdict.get("done") else {
        panic!("no answer")
    };
    assert_eq!(answer.outcome().to_string(), "yes");
}

#[test]
fn a_cancelled_run_returns_at_once_while_its_judge_is_still_asked() {
    let (strategy, evidence) = inline("migrated 41 of 42 tables");
    let (models, server) = slow_jev(Duration::from_secs(2));
    let context = context("cancelled", &models, &[]);
    let cancel = Cancel::new(Source::Harness);
    let canceller = cancel.clone();
    let cancelling = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        canceller.cancel();
    });

    let started = Instant::now();
    let ran = run_strategy(&context, &strategy, &evidence, in_ms(4_000), &cancel);
    let elapsed = started.elapsed();
    cancelling.join().unwrap();
    server.join().unwrap();

    assert!(matches!(ran, Ran::Cancelled), "{ran:?}");
    assert!(elapsed < Duration::from_millis(1_000));
}

#[test]
fn cancelled_integration_jobs_keep_their_slots_and_leave_harness_workers_free() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // Admission caps integration at 8, the pool at 32: the first 8 keep
    // their admitted slots and the rest fill the pool without one.
    let admission = Admission::new(Capacity::DAEMON);
    let release = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(Mutex::new(None::<Cancel>));
    let held = Arc::clone(&release);
    let done = Arc::clone(&finished);
    let cancelling = Arc::clone(&active);
    let models = r#"{ "models": { "jev": { "format": "systemone",
        "endpoint": "http://127.0.0.1:9", "model": "jev", "key": { "env": "TEST" } } },
        "tiers": { "system-one": ["jev"] } }"#;
    let base = Arc::into_inner(context("held-cancel", models, &[])).unwrap();
    let slow = Arc::new(Context {
        keys: Box::new(move |_| {
            cancelling.lock().unwrap().as_ref().unwrap().cancel();
            while !held.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            done.fetch_add(1, Ordering::SeqCst);
            None
        }),
        ..base
    });
    let (strategy, evidence) = inline("waiting");
    let (admitted, busy) =
        hold_integration_workers(&admission, &slow, &strategy, &evidence, &active);

    // The admitted jobs still run, so their eight slots are still held.
    assert_eq!(admission.in_use().total(), 8);

    // Every integration worker is busy, so the next run is refused at once.
    let refused = run_strategy(
        &placeholder("filled", r#"{ "answers": [] }"#),
        &strategy,
        &evidence,
        in_ms(1_000),
        &Cancel::new(Source::Integration),
    );
    assert!(matches!(&refused, Ran::Busy), "{refused:?}");

    // The harness pool is separate, so its strategy still settles.
    let harness = Cancel::new(Source::Harness).admitted(admission.admit(Source::Harness).unwrap());
    let ran = run_strategy(
        &placeholder("unstarved", r#"{ "answers": [] }"#),
        &strategy,
        &evidence,
        in_ms(2_000),
        &harness,
    );
    release.store(true, Ordering::SeqCst);
    *active.lock().unwrap() = None;
    drop(harness);

    assert_eq!((admitted, busy), (8, 24));
    assert!(matches!(ran, Ran::Settled(_)), "{ran:?}");
    let started = Instant::now();
    while admission.in_use().total() != 0 || finished.load(Ordering::SeqCst) != 32 {
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(1));
    }
}
