//! A lexical check for a few shell requests.

use serde_json::Value;

use crate::{Outcome, Request, Rule, Trace, TraceEvent, Verdict};

/// Gives an opinion on the built-in shell checks.
pub struct ShellGuard;

impl Rule for ShellGuard {
    fn name(&self) -> &'static str {
        "shell-guard"
    }

    fn judge(&self, request: &Request, trace: &mut Trace) -> Option<Verdict> {
        if request.field("tool").and_then(Value::as_str) != Some("shell") {
            return None;
        }

        let command = request.field("input.command").and_then(Value::as_str);
        let path = request
            .field("path")
            .and_then(Value::as_str)
            .or_else(|| request.field("input.path").and_then(Value::as_str));

        let (outcome, reason) = if command.is_some_and(removes_root_recursively) {
            (Outcome::Deny, "recursive delete of the root directory")
        } else if [command, path]
            .into_iter()
            .flatten()
            .any(|text| text.contains(".ssh"))
        {
            (Outcome::Ask, "credential path needs a person")
        } else if command.is_some_and(is_git_status) {
            (Outcome::Allow, "git status is safe")
        } else {
            return None;
        };

        let name = self.name();
        trace.push(TraceEvent::RuleOpinion {
            rule: name.to_owned(),
            outcome,
            reason: reason.to_owned(),
        });

        Some(Verdict::new(outcome, name, reason))
    }
}

fn removes_root_recursively(command: &str) -> bool {
    let mut tokens = command.split_whitespace();

    if tokens.next() != Some("rm") {
        return false;
    }

    let mut recursive = false;
    let mut forced = false;
    let mut root = false;

    for token in tokens {
        if let Some(flags) = token.strip_prefix('-')
            && !flags.starts_with('-')
        {
            recursive |= flags.contains('r');
            forced |= flags.contains('f');
        }

        root |= token == "/";
    }

    recursive && forced && root
}

fn is_git_status(command: &str) -> bool {
    let mut tokens = command.split_whitespace();
    tokens.next() == Some("git") && tokens.next() == Some("status")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Outcome, Request, Rule, Trace, TraceEvent, Verdict};

    use super::ShellGuard;

    #[test]
    fn allows_git_status() {
        let subject =
            json!({ "tool": "shell", "input": { "command": "git status" } });

        assert_opinion(&subject, Outcome::Allow, "git status is safe");
    }

    #[test]
    fn abstains_for_git_commit() {
        assert_no_opinion(&json!({
            "tool": "shell",
            "input": { "command": "git commit" }
        }));
    }

    #[test]
    fn abstains_for_mercurial_status() {
        assert_no_opinion(&json!({
            "tool": "shell",
            "input": { "command": "hg status" }
        }));
    }

    #[test]
    fn asks_about_credential_paths_in_the_command_or_path() {
        let subjects = [
            json!({ "tool": "shell", "input": { "command": "ls ~/.ssh" } }),
            json!({ "tool": "shell", "path": "/home/me/.ssh" }),
            json!({ "tool": "shell", "input": { "path": "/home/me/.ssh" } }),
            json!({
                "tool": "shell",
                "input": { "command": "git status", "path": "/home/me/.ssh" }
            }),
        ];

        for subject in &subjects {
            assert_opinion(subject, Outcome::Ask, "credential path needs a person");
        }
    }

    #[test]
    fn denies_recursive_forced_root_removal() {
        for command in ["rm -rf /", "rm -r -f /", "rm -fr /"] {
            let subject =
                json!({ "tool": "shell", "input": { "command": command } });

            assert_opinion(
                &subject,
                Outcome::Deny,
                "recursive delete of the root directory",
            );
        }
    }

    #[test]
    fn abstains_for_another_tool() {
        assert_no_opinion(&json!({
            "tool": "read",
            "path": "/home/me/.ssh",
            "input": { "command": "rm -rf /" }
        }));
    }

    #[test]
    fn abstains_for_removal_of_a_project_path() {
        assert_no_opinion(&json!({
            "tool": "shell",
            "input": { "command": "rm -rf ./build" }
        }));
    }

    fn assert_opinion(subject: &Value, outcome: Outcome, reason: &str) {
        let (verdict, trace) = judge(subject);

        assert_eq!(verdict, Some(Verdict::new(outcome, "shell-guard", reason)));
        assert_eq!(
            trace.events(),
            &[TraceEvent::RuleOpinion {
                rule: "shell-guard".to_owned(),
                outcome,
                reason: reason.to_owned(),
            }]
        );
    }

    fn assert_no_opinion(subject: &Value) {
        let (verdict, trace) = judge(subject);

        assert_eq!(verdict, None);
        assert_eq!(trace.events(), &[]);
    }

    fn judge(subject: &Value) -> (Option<Verdict>, Trace) {
        let json =
            json!({ "session": "s1", "hook": "tool_before", "subject": subject });
        let request = Request::parse(&json.to_string()).unwrap();
        let mut trace = Trace::new();
        let verdict = ShellGuard.judge(&request, &mut trace);

        (verdict, trace)
    }
}
