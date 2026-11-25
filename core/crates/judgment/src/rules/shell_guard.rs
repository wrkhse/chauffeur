//! A demonstration rule for a few exact shell and read requests.
//!
//! `ShellGuard` is a small lexical example, not a complete shell guard. It
//! reads a shell command as plain words and gives an opinion on three
//! patterns only:
//!
//! - **Deny:** `rm` with recursive and force flags and a root target such
//!   as `/`, `'/'`, `/*`, `//` or `/.`, after any `sudo` and
//!   `NAME=value` prefixes.
//! - **Ask:** a path that normalizes into `~/.ssh` or `$HOME/.ssh`, as a
//!   shell command word or as the `path` or `input.path` field of a shell
//!   or read request.
//! - **Allow:** the exact words `git status`, optionally followed by plain
//!   flags made of ASCII letters, digits, `-` and `=`.
//!
//! Before matching any command pattern it abstains on a command that holds
//! a `;`, `|`, `&`, `$`, backtick, `<`, `>`, `(`, `)` or newline. It
//! abstains on everything it does not recognize and leaves those requests
//! to other rules.
//!
//! Known limits, each a request it does not judge:
//!
//! - Globs other than a final `*`, such as `/?` or `~/.s?h`.
//! - Variables and expansions in commands, such as `$HOME` or `$ROOT`.
//! - Other shells and wrappers, such as `sh -c`, `bash -c` or `xargs`.
//! - Other spellings of `rm`, such as `/bin/rm`, and `sudo` options.
//! - Backslash escapes and unterminated quotes.
//! - Home directories spelled as absolute paths, such as `/home/me/.ssh`.
//!
//! A quoted tilde counts as the home directory, so the rule may ask about a
//! literal `~` directory that the shell would leave alone.

use serde_json::Value;

use crate::{Outcome, Request, Rule, Trace, TraceEvent, Verdict};

/// The reason for allowing a literal `git status`.
const ALLOW: &str = "literal git status with only plain flags";
/// The reason for asking about a path in the home `.ssh` directory.
const ASK: &str = "path is in the home .ssh directory";
/// The reason for denying a recursive, forced removal of the root.
const DENY: &str = "rm uses recursive and force flags against a root target";

/// Characters that make a command more than plain words to this rule.
const METACHARACTERS: [char; 10] =
    [';', '|', '&', '$', '`', '<', '>', '(', ')', '\n'];

/// Gives an opinion on the few shell and read patterns the module lists.
pub struct ShellGuard;

impl Rule for ShellGuard {
    fn name(&self) -> &'static str {
        "shell-guard"
    }

    fn judge(&self, request: &Request, trace: &mut Trace) -> Option<Verdict> {
        let tool = request.field("tool").and_then(Value::as_str)?;
        let command = match tool {
            "shell" => request.field("input.command").and_then(Value::as_str),
            "read" => None,
            _ => return None,
        };
        let words = command.and_then(command_words);
        let paths = ["path", "input.path"]
            .into_iter()
            .filter_map(|name| request.field(name).and_then(Value::as_str));

        let (outcome, reason) = if words.as_deref().is_some_and(removes_root) {
            (Outcome::Deny, DENY)
        } else if words
            .iter()
            .flatten()
            .map(String::as_str)
            .chain(paths)
            .any(in_home_ssh)
        {
            (Outcome::Ask, ASK)
        } else if words.is_some() && command.is_some_and(is_git_status) {
            (Outcome::Allow, ALLOW)
        } else {
            return None;
        };

        let name = self.name();
        let verdict = Verdict::new(outcome, name, reason).ok()?;
        let event = TraceEvent::rule_opinion(name, outcome, reason).ok()?;
        trace.push(event);

        Some(verdict)
    }
}

/// Splits a command into words with quotes removed, or abstains with `None`.
///
/// It abstains on any metacharacter first, then on a backslash or an
/// unterminated quote, since it does not model escapes.
fn command_words(command: &str) -> Option<Vec<String>> {
    if command.contains(METACHARACTERS) || command.contains('\\') {
        return None;
    }

    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;

    for character in command.chars() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (None, '\'' | '"') => {
                quote = Some(character);
                word.get_or_insert_with(String::new);
            }
            (None, _) if character.is_ascii_whitespace() => {
                words.extend(word.take());
            }
            _ => {
                word.get_or_insert_with(String::new).push(character);
            }
        }
    }

    if quote.is_some() {
        return None;
    }

    words.extend(word);
    Some(words)
}

/// Whether the words run `rm` with recursive and force flags on a root.
fn removes_root(words: &[String]) -> bool {
    let mut rest = words
        .iter()
        .map(String::as_str)
        .skip_while(|word| *word == "sudo" || is_assignment(word));

    if rest.next() != Some("rm") {
        return false;
    }

    let mut recursive = false;
    let mut forced = false;
    let mut root = false;
    let mut options = true;

    for word in rest {
        if options && word == "--" {
            options = false;
        } else if options && let Some(long) = word.strip_prefix("--") {
            recursive |= long == "recursive";
            forced |= long == "force";
        } else if options && word.len() > 1 && word.starts_with('-') {
            recursive |= word.contains(['r', 'R']);
            forced |= word.contains('f');
        } else {
            root |= is_root(word);
        }
    }

    recursive && forced && root
}

/// Whether a word is a shell variable assignment such as `FOO=1`.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();

    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// Whether a path names the root: `/` with only `/`, `.` and `..`
/// parts, optionally ending in `*`.
fn is_root(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let rest = rest.strip_suffix('*').unwrap_or(rest);

    rest.split('/').all(|part| matches!(part, "" | "." | ".."))
}

/// Whether a path normalizes into `~/.ssh` or `$HOME/.ssh`.
///
/// It resolves `.`, `..` and repeated slashes lexically; a path that
/// leaves the home directory is outside it.
fn in_home_ssh(path: &str) -> bool {
    let Some(rest) = ["~", "$HOME", "${HOME}"]
        .into_iter()
        .find_map(|home| path.strip_prefix(home))
    else {
        return false;
    };

    if !rest.is_empty() && !rest.starts_with('/') {
        return false;
    }

    let mut parts: Vec<&str> = Vec::new();

    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            _ => parts.push(part),
        }
    }

    parts.first() == Some(&".ssh")
}

/// Whether a command is `git status` followed only by plain flags.
///
/// A plain flag starts with `-` and holds at least one letter or digit, so
/// the `--` separator does not count.
fn is_git_status(command: &str) -> bool {
    let mut words = command.split_ascii_whitespace();
    let plain = |character: char| {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '=')
    };

    words.next() == Some("git")
        && words.next() == Some("status")
        && words.all(|word| {
            word.starts_with('-')
                && word.chars().all(plain)
                && word
                    .chars()
                    .any(|character| character.is_ascii_alphanumeric())
        })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Outcome, Request, Rule, Trace, TraceEvent, Verdict};

    use super::{ALLOW, ASK, DENY, ShellGuard};

    #[test]
    fn allows_git_status() {
        let subject =
            json!({ "tool": "shell", "input": { "command": "git status" } });

        assert_opinion(&subject, Outcome::Allow, ALLOW);
        assert_opinion(
            &shell("git\tstatus --porcelain=v2 -sb --untracked-files=no"),
            Outcome::Allow,
            ALLOW,
        );
    }

    #[test]
    fn abstains_for_git_status_with_operands_or_nonplain_flags() {
        for command in [
            "git status README.md",
            "git status -- --short",
            "git status --porcelain=v_2",
            "git status --porcelain='v2'",
            "git status --foo=é",
        ] {
            assert_no_opinion(&shell(command));
        }
    }

    #[test]
    fn checks_for_metacharacters_before_matching_any_command_pattern() {
        for character in [';', '|', '&', '$', '`', '<', '>', '(', ')', '\n'] {
            assert_no_opinion(&shell(&format!("git status {character}")));
            assert_no_opinion(&shell(&format!("rm -rf / {character}")));
        }
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
            shell("ls ~/work/../.ssh/./id_ed25519"),
            // The shell keeps a quoted tilde literal; the rule asks anyway.
            shell("ls '~/.ssh/key with spaces'"),
            json!({ "tool": "shell", "path": "$HOME//.ssh" }),
            json!({ "tool": "shell", "input": { "path": "~/.ssh" } }),
            json!({
                "tool": "shell",
                "input": { "command": "git status", "path": "~/.ssh" }
            }),
            json!({ "tool": "read", "input": { "path": "~/.ssh/id_ed25519" } }),
            json!({ "tool": "read", "path": "$HOME/.ssh/config" }),
        ];

        for subject in &subjects {
            assert_opinion(subject, Outcome::Ask, ASK);
        }
    }

    #[test]
    fn abstains_for_paths_outside_home_ssh() {
        for path in [
            "~/project/.ssh/id_ed25519",
            "~/.ssh-backup/id_ed25519",
            "~/.ssh/../notes",
            "/home/me/.ssh/id_ed25519",
        ] {
            assert_no_opinion(&shell(&format!("ls {path}")));
            assert_no_opinion(&json!({ "tool": "read", "path": path }));
        }

        assert_no_opinion(&shell("echo 'notes about ~/.ssh keys'"));
        assert_no_opinion(&json!({ "tool": "read", "path": "$HOME" }));
    }

    #[test]
    fn denies_recursive_forced_root_removal() {
        for command in [
            "rm -rf /",
            "rm -fr /",
            "rm -Rf /",
            "rm -fR /",
            "rm -r -f /",
            "rm --recursive --force /",
            "rm -rf '/'",
            "rm -rf \"/\"",
            "rm -rf /*",
            "rm -rf //",
            "rm -rf /.",
            "sudo rm -rf /",
            "FOO=1 rm -rf /",
            "VAR=x sudo rm -rf /",
            "rm -rf -- /",
            "rm -rf ~/.ssh /",
        ] {
            assert_opinion(&shell(command), Outcome::Deny, DENY);
        }
    }

    #[test]
    fn abstains_for_the_documented_limits() {
        for command in [
            "ls ~/.s?h",
            "rm -rf /?",
            "ls $HOME/.ssh",
            "rm -rf $ROOT",
            "sh -c 'rm -rf /'",
            "bash -c 'git status'",
            "/bin/rm -rf /",
            "sudo -u root rm -rf /",
            "rm -rf \\/",
            "rm -rf '/",
        ] {
            assert_no_opinion(&shell(command));
        }
    }

    #[test]
    fn abstains_for_another_tool() {
        assert_no_opinion(&json!({
            "tool": "other",
            "path": "~/.ssh/id_ed25519",
            "input": { "command": "rm -rf /" }
        }));
    }

    #[test]
    fn abstains_for_removal_of_a_project_path() {
        for command in ["rm -rf ./build", "rm -r /", "rm -f /", "rm -- -rf /"] {
            assert_no_opinion(&shell(command));
        }
    }

    #[test]
    fn abstains_unless_rm_has_both_long_flags() {
        for command in ["rm --force /", "rm --recursive /"] {
            assert_no_opinion(&shell(command));
        }

        assert_opinion(&shell("rm --force --recursive /"), Outcome::Deny, DENY);
    }

    #[test]
    fn skips_only_assignments_with_a_valid_name() {
        for command in ["_FOO=1 rm -rf /", "A_B=1 rm -rf /", "AB1=1 rm -rf /"] {
            assert_opinion(&shell(command), Outcome::Deny, DENY);
        }

        for command in ["1FOO=1 rm -rf /", "-FOO=1 rm -rf /", "A-B=1 rm -rf /"] {
            assert_no_opinion(&shell(command));
        }
    }

    #[test]
    fn asks_only_when_a_slash_follows_the_tilde() {
        assert_no_opinion(&json!({ "tool": "read", "path": "~.ssh/id_ed25519" }));
        assert_no_opinion(&shell("ls ~.ssh/id_ed25519"));
        assert_opinion(
            &json!({ "tool": "read", "path": "~/.ssh/id_ed25519" }),
            Outcome::Ask,
            ASK,
        );
    }

    fn shell(command: &str) -> Value {
        json!({ "tool": "shell", "input": { "command": command } })
    }

    fn assert_opinion(subject: &Value, outcome: Outcome, reason: &str) {
        let (verdict, trace) = judge(subject);

        let expected = Verdict::new(outcome, "shell-guard", reason)
            .expect("test verdict is valid");
        let event = TraceEvent::rule_opinion("shell-guard", outcome, reason)
            .expect("test event is valid");

        assert_eq!(verdict, Some(expected));
        assert_eq!(trace.events(), &[event]);
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
