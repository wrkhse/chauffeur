//! The answer to a request: an outcome and the rule that chose it.

use serde::{Deserialize, Serialize};

/// What the harness does with the call.
///
/// The declaration order is the precedence: `Allow < Ask < Deny`, so the
/// strictest outcome of several rules is their maximum.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The call goes ahead.
    Allow,
    /// The person decides.
    Ask,
    /// The call is refused.
    Deny,
}

/// An outcome with the rule that decided it and the reason it gives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// What the harness does.
    pub outcome: Outcome,
    /// The name of the deciding rule.
    pub rule: String,
    /// Why the rule decided so, in words for the person.
    pub reason: String,
}

impl Verdict {
    /// Builds a verdict from borrowed rule and reason text.
    #[must_use]
    pub fn new(outcome: Outcome, rule: &str, reason: &str) -> Self {
        Self {
            outcome,
            rule: rule.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Verdict};

    #[test]
    fn outcomes_serialize_in_snake_case() {
        let json = |outcome| serde_json::to_string(&outcome).unwrap();

        assert_eq!(json(Outcome::Allow), "\"allow\"");
        assert_eq!(json(Outcome::Ask), "\"ask\"");
        assert_eq!(json(Outcome::Deny), "\"deny\"");
    }

    #[test]
    fn deny_outranks_ask_outranks_allow() {
        assert_eq!(
            Outcome::Deny.cmp(&Outcome::Ask),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            Outcome::Ask.cmp(&Outcome::Allow),
            std::cmp::Ordering::Greater
        );

        let strictest = [Outcome::Ask, Outcome::Deny, Outcome::Allow]
            .into_iter()
            .max();

        assert_eq!(strictest, Some(Outcome::Deny));
    }

    #[test]
    fn new_copies_its_fields() {
        let verdict = Verdict::new(Outcome::Ask, "force-push", "rewrites history");

        assert_eq!(verdict.outcome, Outcome::Ask);
        assert_eq!(verdict.rule, "force-push");
        assert_eq!(verdict.reason, "rewrites history");
    }
}
