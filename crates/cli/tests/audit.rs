//! `chauffeur audit` under a reader that stops early, as `head` does.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// A state folder of its own, removed when dropped.
struct State(PathBuf);

impl State {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("chauffeur-audit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Appends `records` decisions to the audit log, each a long line.
    fn log(&self, records: usize) {
        let record = serde_json::json!({
            "at": 3_661,
            "agent_id": "ses_test",
            "signal": "tool_result",
            "detail": "x".repeat(200),
            "effects": [],
            "trace": { "elapsed_ms": 1, "questions": [] }
        });
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.0.join("audit.jsonl"))
            .unwrap()
            .write_all(format!("{record}\n").repeat(records).as_bytes())
            .unwrap();
    }

    fn audit(&self, args: &[&str]) -> Audit {
        let child = Command::new(env!("CARGO_BIN_EXE_chauffeur"))
            .arg("audit")
            .args(args)
            .env("CHAUFFEUR_STATE_DIR", &self.0)
            .env("CHAUFFEUR_CONFIG_DIR", self.0.join("config"))
            .env("CHAUFFEUR_SOURCEFED", "off")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Audit(child)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running `chauffeur audit`, killed if the test fails before it exits.
struct Audit(Child);

impl Audit {
    /// Reads one line, then closes stdout. A silent child is killed and
    /// reaped before joining the reader and returning the timeout error.
    fn first_line(&mut self, limit: Duration) -> Result<String, String> {
        let stdout = self.0.stdout.take().unwrap();
        let (done, finished) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(stdout).read_line(&mut line);
            let _ = done.send(());
            (read, line)
        });
        let ready = finished.recv_timeout(limit);

        if ready.is_err() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }

        let (read, line) = reader.join().expect("the audit reader panicked");
        ready.map_err(|error| format!("no audit line within {limit:?}: {error}"))?;
        read.map_err(|error| format!("read the audit line: {error}"))?;

        Ok(line)
    }

    /// How it exited, failing the test if it runs past `limit`.
    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(started.elapsed() < limit, "still running after {limit:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }
}

impl Drop for Audit {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn audit_stops_quietly_when_its_reader_closes_the_pipe() {
    let state = State::new("closed-pipe");
    // About 250 KB printed: far more than a pipe holds, so audit is still
    // printing when it closes.
    state.log(1_000);
    let mut audit = state.audit(&["1000"]);

    assert!(
        audit
            .first_line(Duration::from_secs(5))
            .unwrap()
            .contains("tool_result")
    );
    let status = audit.exit_within(Duration::from_secs(5));
    let stderr = audit.stderr();

    assert!(status.success(), "{status:?}: {stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn a_silent_follower_is_killed_and_reaped_when_the_first_line_times_out() {
    let state = State::new("silent-follow");
    state.log(1);
    let mut follower = state.audit(&["0", "--follow"]);

    let error = follower.first_line(Duration::from_millis(200)).unwrap_err();

    assert!(error.contains("no audit line within"), "{error}");
    assert!(follower.0.try_wait().unwrap().is_some());
    assert!(follower.stderr().is_empty());
}

#[test]
fn audit_follow_stops_quietly_when_its_reader_closes_the_pipe() {
    let state = State::new("closed-follow");
    state.log(1);
    let mut follower = state.audit(&["1", "--follow"]);

    assert!(
        follower
            .first_line(Duration::from_secs(5))
            .unwrap()
            .contains("tool_result")
    );
    // Following notices the closed pipe only at its next write.
    state.log(1);
    let status = follower.exit_within(Duration::from_secs(5));
    let stderr = follower.stderr();

    assert!(status.success(), "{status:?}: {stderr}");
    assert!(stderr.is_empty(), "{stderr}");
}
