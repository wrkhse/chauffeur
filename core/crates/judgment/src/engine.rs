//! The engine combines rule opinions and records the decision.

use crate::{Outcome, Request, RuleSet, Trace, TraceEvent, Verdict};

/// The verdict and trace for a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judgment {
    /// The id of the request the engine judges.
    pub request_id: String,
    /// The strictest opinion, or the default when all rules abstain.
    pub verdict: Verdict,
    /// The events the engine and rules record.
    pub trace: Trace,
}

/// Evaluates a bounded, ordered set of rules.
pub struct Engine {
    rules: RuleSet,
}

impl Engine {
    /// Creates an engine with the supplied rule order.
    #[must_use]
    pub const fn new(rules: RuleSet) -> Self {
        Self { rules }
    }

    /// Keeps the strictest outcome. The first rule wins a tie.
    #[must_use]
    pub fn judge(&self, request: &Request) -> Judgment {
        let mut trace = Trace::new();
        let mut best: Option<Verdict> = None;

        for rule in self.rules.iter() {
            trace.push(TraceEvent::RuleStarted {
                rule: rule.name().to_owned(),
            });

            if let Some(opinion) = rule.judge(request, &mut trace)
                && best
                    .as_ref()
                    .is_none_or(|winner| opinion.outcome > winner.outcome)
            {
                best = Some(opinion);
            }
        }

        let default =
            || Verdict::new(Outcome::Allow, "none", "no rule had an opinion");
        let verdict = best.unwrap_or_else(default);
        trace.push(TraceEvent::Decided {
            rule: verdict.rule.clone(),
            outcome: verdict.outcome,
        });

        Judgment {
            request_id: request.id().to_owned(),
            verdict,
            trace,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Outcome, Request, Rule, RuleSet, Trace, TraceEvent, Verdict};

    use super::Engine;

    #[test]
    fn allows_git_status_and_records_the_opinion() {
        let engine = Engine::new(RuleSet::builtin());
        let judgment = engine.judge(&shell_request("git status"));

        assert_eq!(judgment.request_id, "call-1");
        assert_eq!(
            judgment.verdict,
            Verdict::new(Outcome::Allow, "shell-guard", "git status is safe")
        );
        assert_eq!(
            judgment.trace.events(),
            &[
                TraceEvent::RuleStarted {
                    rule: "shell-guard".to_owned(),
                },
                TraceEvent::RuleOpinion {
                    rule: "shell-guard".to_owned(),
                    outcome: Outcome::Allow,
                    reason: "git status is safe".to_owned(),
                },
                TraceEvent::Decided {
                    rule: "shell-guard".to_owned(),
                    outcome: Outcome::Allow,
                },
            ]
        );
    }

    #[test]
    fn asks_about_a_credential_path() {
        let engine = Engine::new(RuleSet::builtin());
        let judgment = engine.judge(&request(&json!({
            "tool": "shell",
            "path": "/home/me/.ssh"
        })));

        assert_eq!(
            judgment.verdict,
            Verdict::new(
                Outcome::Ask,
                "shell-guard",
                "credential path needs a person"
            )
        );
    }

    #[test]
    fn denies_recursive_root_removal() {
        let engine = Engine::new(RuleSet::builtin());
        let judgment = engine.judge(&shell_request("rm -rf /"));

        assert_eq!(
            judgment.verdict,
            Verdict::new(
                Outcome::Deny,
                "shell-guard",
                "recursive delete of the root directory",
            )
        );
    }

    #[test]
    fn allows_by_default_and_records_that_no_rule_opines() {
        let engine = Engine::new(RuleSet::builtin());
        let judgment = engine.judge(&request(&json!({ "tool": "read" })));

        assert_eq!(
            judgment.verdict,
            Verdict::new(Outcome::Allow, "none", "no rule had an opinion")
        );
        assert_eq!(
            judgment.trace.events(),
            &[
                TraceEvent::RuleStarted {
                    rule: "shell-guard".to_owned(),
                },
                TraceEvent::Decided {
                    rule: "none".to_owned(),
                    outcome: Outcome::Allow,
                },
            ]
        );
    }

    #[test]
    fn later_deny_beats_shell_guard_allow() {
        let mut rules = RuleSet::builtin();
        rules
            .insert(Box::new(FixedRule("deny", Outcome::Deny)))
            .unwrap();
        let judgment = Engine::new(rules).judge(&shell_request("git status"));

        assert_eq!(
            judgment.verdict,
            Verdict::new(Outcome::Deny, "deny", "fixed opinion")
        );
    }

    #[test]
    fn earlier_deny_beats_later_allow() {
        let mut rules = RuleSet::builtin();
        rules
            .insert(Box::new(FixedRule("allow", Outcome::Allow)))
            .unwrap();
        let judgment = Engine::new(rules).judge(&shell_request("rm -rf /"));

        assert_eq!(
            judgment.verdict,
            Verdict::new(
                Outcome::Deny,
                "shell-guard",
                "recursive delete of the root directory",
            )
        );
    }

    #[test]
    fn first_rule_wins_a_tie() {
        let mut rules = RuleSet::builtin();
        rules
            .insert(Box::new(FixedRule("allow", Outcome::Allow)))
            .unwrap();
        let judgment = Engine::new(rules).judge(&shell_request("git status"));

        assert_eq!(
            judgment.verdict,
            Verdict::new(Outcome::Allow, "shell-guard", "git status is safe")
        );
    }

    struct FixedRule(&'static str, Outcome);

    impl Rule for FixedRule {
        fn name(&self) -> &'static str {
            self.0
        }

        fn judge(&self, _: &Request, trace: &mut Trace) -> Option<Verdict> {
            trace.push(TraceEvent::RuleOpinion {
                rule: self.name().to_owned(),
                outcome: self.1,
                reason: "fixed opinion".to_owned(),
            });

            Some(Verdict::new(self.1, self.name(), "fixed opinion"))
        }
    }

    fn shell_request(command: &str) -> Request {
        request(&json!({ "tool": "shell", "input": { "command": command } }))
    }

    fn request(subject: &Value) -> Request {
        let json = json!({
            "id": "call-1",
            "session": "s1",
            "hook": "tool_before",
            "subject": subject
        });

        Request::parse(&json.to_string()).unwrap()
    }
}
