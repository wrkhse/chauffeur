//! The engine combines rule opinions and records the decision.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    MAX_TEXT_BYTES, Request, Rule, RuleSet, Trace, TraceError, TraceEvent, Verdict,
};

/// Why a judgment is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgmentError {
    /// The request id is empty.
    EmptyRequestId,
    /// A part of the judgment exceeds its byte limit.
    Limit {
        /// The part that exceeds it.
        name: &'static str,
        /// The byte limit.
        max: u64,
    },
}

impl fmt::Display for JudgmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRequestId => f.write_str("request id is empty"),
            Self::Limit { name, max } => write!(f, "{name} exceeds {max}"),
        }
    }
}

impl std::error::Error for JudgmentError {}

/// The verdict and trace for a request.
///
/// The verdict is absent when no rule has an opinion. The engine does not
/// choose an outcome for that case: the host decides what no opinion means,
/// such as falling through to its own permission prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Judgment {
    request_id: String,
    verdict: Option<Verdict>,
    trace: Trace,
}

impl Judgment {
    /// Builds a judgment for a request id.
    ///
    /// # Errors
    ///
    /// Returns [`JudgmentError::EmptyRequestId`] for an empty id and
    /// [`JudgmentError::Limit`] for an id over [`MAX_TEXT_BYTES`].
    pub fn new(
        request_id: &str,
        verdict: Option<Verdict>,
        trace: Trace,
    ) -> Result<Self, JudgmentError> {
        validate(request_id)?;

        Ok(Self {
            request_id: request_id.to_owned(),
            verdict,
            trace,
        })
    }

    /// The id of the request the engine judges.
    #[must_use]
    pub const fn request_id(&self) -> &str {
        self.request_id.as_str()
    }

    /// The strictest opinion, or `None` when no rule has one.
    #[must_use]
    pub const fn verdict(&self) -> Option<&Verdict> {
        self.verdict.as_ref()
    }

    /// The events the engine and rules record.
    #[must_use]
    pub const fn trace(&self) -> &Trace {
        &self.trace
    }
}

#[derive(Deserialize)]
#[serde(rename = "Judgment", deny_unknown_fields)]
struct JudgmentFile {
    request_id: String,
    verdict: Option<Verdict>,
    trace: Trace,
}

impl TryFrom<JudgmentFile> for Judgment {
    type Error = JudgmentError;

    fn try_from(file: JudgmentFile) -> Result<Self, Self::Error> {
        validate(&file.request_id)?;

        Ok(Self {
            request_id: file.request_id,
            verdict: file.verdict,
            trace: file.trace,
        })
    }
}

impl<'de> Deserialize<'de> for Judgment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(JudgmentFile::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

fn validate(request_id: &str) -> Result<(), JudgmentError> {
    if request_id.is_empty() {
        return Err(JudgmentError::EmptyRequestId);
    }

    if request_id.len() > MAX_TEXT_BYTES {
        return Err(JudgmentError::Limit {
            name: "request id",
            max: u64::try_from(MAX_TEXT_BYTES).unwrap_or(u64::MAX),
        });
    }

    Ok(())
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

    /// Keeps the strictest outcome; the first rule wins a tie.
    ///
    /// The verdict is `None` when every rule abstains, and the trace then
    /// records no decision. The trace always ends with a finished event.
    #[must_use]
    pub fn judge(&self, request: &Request) -> Judgment {
        let mut trace = Trace::new();
        let mut best: Option<Verdict> = None;

        for rule in self.rules.iter() {
            record(&mut trace, TraceEvent::rule_started(rule.name()));

            let opinion = rule.judge(request, &mut trace);

            if let Some(opinion) = opinion.and_then(|o| named_by(rule, o))
                && best
                    .as_ref()
                    .is_none_or(|winner| opinion.outcome() > winner.outcome())
            {
                best = Some(opinion);
            }
        }

        if let Some(verdict) = &best {
            let decided = TraceEvent::decided(verdict.rule(), verdict.outcome());
            record(&mut trace, decided);
        }

        trace.finish();

        // A parsed request's id is non-empty and within MAX_TEXT_BYTES.
        debug_assert!(validate(request.id()).is_ok());

        Judgment {
            request_id: request.id().to_owned(),
            verdict: best,
            trace,
        }
    }
}

/// Keeps an opinion only when its verdict names the rule that gives it.
///
/// A misnamed opinion fails a debug assertion and is skipped in a release
/// build, so a verdict never credits a rule that did not decide.
fn named_by(rule: &dyn Rule, opinion: Verdict) -> Option<Verdict> {
    let named = opinion.rule() == rule.name();
    debug_assert!(named, "a rule's verdict names the rule that gives it");

    named.then_some(opinion)
}

/// Keeps an engine event built from text a rule set or verdict validates.
fn record(trace: &mut Trace, event: Result<TraceEvent, TraceError>) {
    debug_assert!(event.is_ok(), "validated rule text is a valid trace event");

    if let Ok(event) = event {
        trace.push(event);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        MAX_EVENTS, MAX_TEXT_BYTES, Outcome, Request, Rule, RuleSet, Trace,
        TraceEvent, Verdict,
    };

    use super::{Engine, Judgment, JudgmentError};

    const SAFE_STATUS: &str = "literal git status with only plain flags";
    const SSH_PATH: &str = "path is in the home .ssh directory";
    const ROOT_DELETE: &str =
        "rm uses recursive and force flags against a root target";
    const FIXED: &str = "fixed opinion";

    #[test]
    fn allows_git_status_and_records_each_step() {
        let judgment = builtin().judge(&shell_request("git status"));

        assert_eq!(judgment.request_id(), "call-1");
        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Allow, "shell-guard", SAFE_STATUS))
        );
        assert_eq!(
            judgment.trace().events(),
            &[
                started("shell-guard"),
                opinion("shell-guard", Outcome::Allow, SAFE_STATUS),
                decided("shell-guard", Outcome::Allow),
                finished(),
            ]
        );
        assert_eq!(judgment.trace().dropped(), 0);
    }

    #[test]
    fn asks_about_a_credential_path() {
        let subject = json!({ "tool": "shell", "path": "~/.ssh" });
        let judgment = builtin().judge(&request(&subject));

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Ask, "shell-guard", SSH_PATH))
        );
    }

    #[test]
    fn denies_recursive_root_removal() {
        let judgment = builtin().judge(&shell_request("rm -rf /"));

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Deny, "shell-guard", ROOT_DELETE))
        );
    }

    #[test]
    fn has_no_verdict_and_no_decision_when_no_rule_opines() {
        let judgment = builtin().judge(&request(&json!({ "tool": "read" })));

        assert_eq!(judgment.verdict(), None);
        assert_eq!(
            judgment.trace().events(),
            &[started("shell-guard"), finished()]
        );
    }

    #[test]
    fn finishes_an_empty_rule_set_without_a_verdict() {
        let judgment = Engine::new(RuleSet::new()).judge(&shell_request("ls"));

        assert_eq!(judgment.verdict(), None);
        assert_eq!(judgment.trace().events(), &[finished()]);
    }

    #[test]
    fn deny_beats_ask_and_records_the_sequence() {
        let judgment = fixed(&[("ask", Outcome::Ask), ("deny", Outcome::Deny)])
            .judge(&shell_request("ls"));

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Deny, "deny", FIXED))
        );
        assert_eq!(
            judgment.trace().events(),
            &[
                started("ask"),
                opinion("ask", Outcome::Ask, FIXED),
                started("deny"),
                opinion("deny", Outcome::Deny, FIXED),
                decided("deny", Outcome::Deny),
                finished(),
            ]
        );
    }

    #[test]
    fn ask_beats_a_later_and_an_earlier_allow() {
        for rules in [
            [("ask", Outcome::Ask), ("allow", Outcome::Allow)],
            [("allow", Outcome::Allow), ("ask", Outcome::Ask)],
        ] {
            let judgment = fixed(&rules).judge(&shell_request("ls"));

            assert_eq!(
                judgment.verdict(),
                Some(&verdict(Outcome::Ask, "ask", FIXED))
            );
        }
    }

    #[test]
    fn first_rule_wins_a_tie() {
        let judgment = fixed(&[("one", Outcome::Ask), ("two", Outcome::Ask)])
            .judge(&shell_request("ls"));

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Ask, "one", FIXED))
        );
    }

    #[test]
    fn every_order_of_three_rules_gives_the_same_outcome() {
        let allow = ("allow", Outcome::Allow);
        let ask = ("ask", Outcome::Ask);
        let deny = ("deny", Outcome::Deny);
        let orders = [
            [allow, ask, deny],
            [allow, deny, ask],
            [ask, allow, deny],
            [ask, deny, allow],
            [deny, allow, ask],
            [deny, ask, allow],
        ];

        for order in orders {
            let judgment = fixed(&order).judge(&shell_request("ls"));
            let events = judgment.trace().events();

            assert_eq!(
                judgment.verdict(),
                Some(&verdict(Outcome::Deny, "deny", FIXED))
            );
            assert_eq!(events.len(), 8);
            assert_eq!(&events[6..], &[decided("deny", Outcome::Deny), finished()]);
        }
    }

    // A debug build stops at the invariant; a release build skips the
    // opinion and lets the other rules decide.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "a rule's verdict names the rule that gives it")
    )]
    fn ignores_an_opinion_that_names_another_rule() {
        let mut rules = RuleSet::new();
        rules
            .insert(Box::new(FixedRule("allow", Outcome::Allow)))
            .expect("valid rule");
        rules.insert(Box::new(MisnamedRule)).expect("valid rule");
        let judgment = Engine::new(rules).judge(&shell_request("ls"));

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Allow, "allow", FIXED))
        );
        assert_eq!(
            judgment.trace().events(),
            &[
                started("allow"),
                opinion("allow", Outcome::Allow, FIXED),
                started("misnamed"),
                decided("allow", Outcome::Allow),
                finished(),
            ]
        );
    }

    #[test]
    fn a_noisy_rule_cannot_take_the_final_slot() {
        let mut rules = RuleSet::new();
        rules.insert(Box::new(NoisyRule)).expect("valid rule");
        let judgment = Engine::new(rules).judge(&shell_request("ls"));
        let events = judgment.trace().events();

        assert_eq!(
            judgment.verdict(),
            Some(&verdict(Outcome::Ask, "noisy", FIXED))
        );
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events.last(), Some(&finished()));
        // 300 opinions and the decision compete for the 254 slots left
        // after the start event and before the reserved final slot.
        assert_eq!(judgment.trace().dropped(), 47);
    }

    #[test]
    fn serializes_and_reads_back_a_judgment() {
        let judgment = builtin().judge(&request(&json!({ "tool": "read" })));
        let json = serde_json::to_value(&judgment).unwrap();
        let events = json!([
            { "kind": "rule_started", "rule": "shell-guard" },
            { "kind": "finished" },
        ]);

        assert_eq!(
            json,
            json!({
                "request_id": "call-1",
                "verdict": null,
                "trace": { "events": events, "dropped": 0 },
            })
        );

        let file = json!({
            "request_id": "call-1",
            "verdict": null,
            "trace": { "events": events },
        });

        assert_eq!(serde_json::from_value::<Judgment>(file).unwrap(), judgment);
    }

    #[test]
    fn round_trips_a_judgment_with_dropped_trace_events() {
        let mut trace = Trace::new();

        for _ in 0..MAX_EVENTS {
            trace.push(opinion("noisy", Outcome::Ask, FIXED));
        }

        trace.finish();
        assert_eq!(trace.dropped(), 1);

        let verdict = verdict(Outcome::Ask, "noisy", FIXED);
        let judgment = Judgment::new("call-1", Some(verdict), trace).unwrap();

        let json = serde_json::to_string(&judgment).unwrap();
        let decoded: Judgment = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, judgment);
    }

    #[test]
    fn refuses_an_empty_request_id_in_new_and_json() {
        let file = json!({
            "request_id": "",
            "verdict": null,
            "trace": { "events": [] },
        });
        let error = serde_json::from_value::<Judgment>(file).unwrap_err();

        assert_eq!(
            Judgment::new("", None, Trace::new()),
            Err(JudgmentError::EmptyRequestId)
        );
        assert_eq!(error.to_string(), "request id is empty");
    }

    #[test]
    fn keeps_a_request_id_at_the_text_limit() {
        let id = "i".repeat(MAX_TEXT_BYTES);
        let judgment = Judgment::new(&id, None, Trace::new()).unwrap();

        assert_eq!(judgment.request_id(), id);
    }

    #[test]
    fn refuses_a_request_id_over_the_text_limit() {
        let id = "i".repeat(MAX_TEXT_BYTES + 1);
        let error = Judgment::new(&id, None, Trace::new()).unwrap_err();

        assert_eq!(
            error,
            JudgmentError::Limit {
                name: "request id",
                max: 16_384
            }
        );
        assert_eq!(error.to_string(), "request id exceeds 16384");
    }

    #[test]
    fn refuses_an_unknown_judgment_field() {
        let file = json!({
            "request_id": "call-1",
            "verdict": null,
            "trace": { "events": [] },
            "extra": 1,
        });
        let error = serde_json::from_value::<Judgment>(file).unwrap_err();

        assert_eq!(
            error.to_string(),
            "unknown field `extra`, expected one of `request_id`, \
             `verdict`, `trace`"
        );
    }

    struct FixedRule(&'static str, Outcome);

    impl Rule for FixedRule {
        fn name(&self) -> &'static str {
            self.0
        }

        fn judge(&self, _: &Request, trace: &mut Trace) -> Option<Verdict> {
            trace.push(opinion(self.0, self.1, FIXED));

            Some(verdict(self.1, self.0, FIXED))
        }
    }

    struct MisnamedRule;

    impl Rule for MisnamedRule {
        fn name(&self) -> &'static str {
            "misnamed"
        }

        fn judge(&self, _: &Request, _: &mut Trace) -> Option<Verdict> {
            Some(verdict(Outcome::Deny, "allow", FIXED))
        }
    }

    struct NoisyRule;

    impl Rule for NoisyRule {
        fn name(&self) -> &'static str {
            "noisy"
        }

        fn judge(&self, _: &Request, trace: &mut Trace) -> Option<Verdict> {
            for _ in 0..300 {
                trace.push(opinion("noisy", Outcome::Ask, FIXED));
            }

            Some(verdict(Outcome::Ask, "noisy", FIXED))
        }
    }

    fn builtin() -> Engine {
        Engine::new(RuleSet::builtin())
    }

    fn fixed(rules: &[(&'static str, Outcome)]) -> Engine {
        let mut set = RuleSet::new();

        for &(name, outcome) in rules {
            set.insert(Box::new(FixedRule(name, outcome)))
                .expect("valid rule");
        }

        Engine::new(set)
    }

    fn verdict(outcome: Outcome, rule: &str, reason: &str) -> Verdict {
        Verdict::new(outcome, rule, reason).expect("valid verdict")
    }

    fn started(rule: &str) -> TraceEvent {
        TraceEvent::rule_started(rule).expect("valid event")
    }

    fn opinion(rule: &str, outcome: Outcome, reason: &str) -> TraceEvent {
        TraceEvent::rule_opinion(rule, outcome, reason).expect("valid event")
    }

    fn decided(rule: &str, outcome: Outcome) -> TraceEvent {
        TraceEvent::decided(rule, outcome).expect("valid event")
    }

    fn finished() -> TraceEvent {
        TraceEvent::finished().expect("valid event")
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
