#![cfg(test)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{self, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, str};

use chauffeur_judgment::MAX_REQUEST_BYTES;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_chauffeur-core");
const ALLOW_REASON: &str = "literal git status with only plain flags";
const ASK_REASON: &str = "path is in the home .ssh directory";
const DENY_REASON: &str = "rm uses recursive and force flags against a root target";

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let counter = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("chauffeur-core-cli-{}-{counter}", process::id());
        let path = env::temp_dir().join(name);
        fs::create_dir(&path).expect("create test state directory");

        Self(path)
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = Command::new(BIN);
        command.args(args).env("CHAUFFEUR_CORE_STATE_DIR", &self.0);

        let Some(input) = input else {
            return command.output().expect("run chauffeur-core");
        };

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn chauffeur-core");

        let written = child.stdin.take().expect("piped stdin").write_all(input);
        let result = child.wait_with_output();

        written.expect("write CLI stdin");
        result.expect("wait for chauffeur-core")
    }

    fn judge(&self, id: &str, subject: &Value) -> Value {
        let request = json!({
            "id": id,
            "session": "s1",
            "hook": "tool_after",
            "subject": subject,
        });
        let output = self.run(&["judge"], Some(request.to_string().as_bytes()));

        serde_json::from_str(stdout(&output)).expect("parse judgment JSON")
    }

    fn audit(&self, args: &[&str]) -> Vec<Value> {
        let output = self.run(args, None);

        stdout(&output)
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse audit JSON line"))
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let removed = fs::remove_dir_all(&self.0);

        if !std::thread::panicking() {
            removed.expect("remove test state directory");
        }
    }
}

fn stdout(output: &Output) -> &str {
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    str::from_utf8(&output.stdout).expect("stdout is UTF-8")
}

fn shell(command: &str) -> Value {
    json!({"tool": "shell", "input": {"command": command}})
}

fn expected_judgment(id: &str, outcome: &str, reason: &str) -> Value {
    json!({
        "request_id": id,
        "verdict": {"outcome": outcome, "rule": "shell-guard", "reason": reason},
        "trace": {
            "events": [
                {"kind": "rule_started", "rule": "shell-guard"},
                {
                    "kind": "rule_opinion",
                    "rule": "shell-guard",
                    "outcome": outcome,
                    "reason": reason,
                },
                {"kind": "decided", "rule": "shell-guard", "outcome": outcome},
                {"kind": "finished"},
            ],
            "dropped": 0,
        },
    })
}

#[test]
fn allows_git_status_and_records_its_judgment() {
    let state = Scratch::new();
    let judgment = state.judge("status-1", &shell("git status"));
    let records = state.audit(&["audit", "1"]);

    assert_eq!(
        judgment,
        expected_judgment("status-1", "allow", ALLOW_REASON)
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["request_id"], judgment["request_id"]);
    assert_eq!(records[0]["session"], json!("s1"));
    assert_eq!(records[0]["hook"], json!("tool_after"));
    assert_eq!(records[0]["judgment"], judgment);
}

#[test]
fn asks_before_reading_a_private_ssh_key() {
    let state = Scratch::new();
    let read = json!({"tool": "read", "input": {"path": "~/.ssh/id_ed25519"}});

    assert_eq!(
        state.judge("ssh-1", &read),
        expected_judgment("ssh-1", "ask", ASK_REASON),
    );
}

#[test]
fn denies_a_recursive_root_removal() {
    let state = Scratch::new();

    assert_eq!(
        state.judge("root-1", &shell("rm -rf /")),
        expected_judgment("root-1", "deny", DENY_REASON),
    );
}

#[test]
fn lists_the_builtin_rules() {
    let state = Scratch::new();

    assert_eq!(stdout(&state.run(&["rules"], None)), "shell-guard\n");
}

#[test]
fn audit_keeps_records_from_separate_runs_in_order() {
    let state = Scratch::new();
    let first = state.judge("status-1", &shell("git status"));
    let second = state.judge("status-2", &shell("git status --short"));
    let records = state.audit(&["audit"]);
    let judgments: Vec<_> =
        records.iter().map(|record| &record["judgment"]).collect();
    let recent = state.audit(&["audit", "1"]);

    assert_eq!(judgments, vec![&first, &second]);
    assert_eq!(&recent, &records[1..]);
}

#[test]
fn rejects_oversized_stdin() {
    let state = Scratch::new();
    let bytes = vec![b' '; MAX_REQUEST_BYTES + 1];
    let output = state.run(&["judge"], Some(&bytes));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(
        str::from_utf8(&output.stderr).expect("stderr is UTF-8"),
        format!("Error: request bytes exceeds {MAX_REQUEST_BYTES}\n"),
    );
}

#[test]
fn rejects_an_unknown_subcommand() {
    let state = Scratch::new();
    let output = state.run(&["unknown"], None);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    assert_eq!(
        output.stderr,
        b"Error: usage: chauffeur-core judge | rules | audit [N] | version\n",
    );
}

#[test]
fn prints_the_workspace_version() {
    let state = Scratch::new();

    assert_eq!(
        stdout(&state.run(&["version"], None)),
        concat!(env!("CARGO_PKG_VERSION"), "\n"),
    );
}
