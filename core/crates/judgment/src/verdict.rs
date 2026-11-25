//! The answer to a request: an outcome and the rule that chose it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// The most bytes in a deciding rule's name.
pub const MAX_RULE_NAME_BYTES: usize = 64;
/// The most bytes in a verdict's reason.
pub const MAX_REASON_BYTES: usize = 256;

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

/// Why a verdict is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictError {
    /// The rule name is empty.
    EmptyRule,
    /// A part of the verdict exceeds its byte limit.
    Limit {
        /// The part that exceeds it.
        name: &'static str,
        /// The byte limit.
        max: u64,
    },
}

impl fmt::Display for VerdictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRule => f.write_str("rule is empty"),
            Self::Limit { name, max } => write!(f, "{name} exceeds {max}"),
        }
    }
}

impl std::error::Error for VerdictError {}

/// An outcome with the rule that decides it and the reason it gives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verdict {
    outcome: Outcome,
    rule: String,
    reason: String,
}

impl Verdict {
    /// Builds a verdict from borrowed rule and reason text.
    ///
    /// # Errors
    ///
    /// Returns [`VerdictError::EmptyRule`] for an empty rule name and
    /// [`VerdictError::Limit`] if the rule name or reason exceeds its byte limit.
    pub fn new(
        outcome: Outcome,
        rule: &str,
        reason: &str,
    ) -> Result<Self, VerdictError> {
        validate(rule, reason)?;

        Ok(Self {
            outcome,
            rule: rule.to_owned(),
            reason: reason.to_owned(),
        })
    }

    /// What the harness does.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// The name of the deciding rule.
    #[must_use]
    pub const fn rule(&self) -> &str {
        self.rule.as_str()
    }

    /// Why the rule chooses this outcome, in words for the person.
    #[must_use]
    pub const fn reason(&self) -> &str {
        self.reason.as_str()
    }
}

#[derive(Deserialize)]
#[serde(rename = "Verdict", deny_unknown_fields)]
struct VerdictFile {
    outcome: Outcome,
    rule: String,
    reason: String,
}

impl TryFrom<VerdictFile> for Verdict {
    type Error = VerdictError;

    fn try_from(file: VerdictFile) -> Result<Self, Self::Error> {
        validate(&file.rule, &file.reason)?;

        Ok(Self {
            outcome: file.outcome,
            rule: file.rule,
            reason: file.reason,
        })
    }
}

impl<'de> Deserialize<'de> for Verdict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(VerdictFile::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

fn validate(rule: &str, reason: &str) -> Result<(), VerdictError> {
    if rule.is_empty() {
        return Err(VerdictError::EmptyRule);
    }

    if rule.len() > MAX_RULE_NAME_BYTES {
        return Err(VerdictError::Limit {
            name: "rule",
            max: u64::try_from(MAX_RULE_NAME_BYTES).unwrap_or(u64::MAX),
        });
    }
    if reason.len() > MAX_REASON_BYTES {
        return Err(VerdictError::Limit {
            name: "reason",
            max: u64::try_from(MAX_REASON_BYTES).unwrap_or(u64::MAX),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MAX_REASON_BYTES, MAX_RULE_NAME_BYTES, Outcome, Verdict, VerdictError,
    };

    #[test]
    fn each_outcome_serializes_in_the_verdict() {
        let serialize = |outcome| {
            let verdict = Verdict::new(outcome, "shell-guard", "reason").unwrap();

            serde_json::to_string(&verdict).unwrap()
        };

        assert_eq!(
            serialize(Outcome::Allow),
            r#"{"outcome":"allow","rule":"shell-guard","reason":"reason"}"#
        );
        assert_eq!(
            serialize(Outcome::Ask),
            r#"{"outcome":"ask","rule":"shell-guard","reason":"reason"}"#
        );
        assert_eq!(
            serialize(Outcome::Deny),
            r#"{"outcome":"deny","rule":"shell-guard","reason":"reason"}"#
        );
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
    fn refuses_an_empty_rule() {
        assert_eq!(
            Verdict::new(Outcome::Ask, "", "reason"),
            Err(VerdictError::EmptyRule)
        );
    }

    #[test]
    fn displays_each_verdict_error() {
        let limit = VerdictError::Limit {
            name: "rule",
            max: 64,
        };

        assert_eq!(VerdictError::EmptyRule.to_string(), "rule is empty");
        assert_eq!(limit.to_string(), "rule exceeds 64");
    }

    #[test]
    fn refuses_a_rule_over_the_byte_limit() {
        let mut rule = "é".repeat(MAX_RULE_NAME_BYTES / 2);
        rule.push('x');

        assert_eq!(
            Verdict::new(Outcome::Ask, &rule, ""),
            Err(VerdictError::Limit {
                name: "rule",
                max: 64
            })
        );
    }

    #[test]
    fn refuses_a_reason_over_the_byte_limit() {
        let mut reason = "é".repeat(MAX_REASON_BYTES / 2);
        reason.push('x');

        assert_eq!(
            Verdict::new(Outcome::Ask, "shell-guard", &reason),
            Err(VerdictError::Limit {
                name: "reason",
                max: 256
            })
        );
    }

    #[test]
    fn round_trips_a_verdict_at_the_byte_limits() {
        let rule = "é".repeat(MAX_RULE_NAME_BYTES / 2);
        let reason = "é".repeat(MAX_REASON_BYTES / 2);
        let verdict = Verdict::new(Outcome::Ask, &rule, &reason).unwrap();
        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: Verdict = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, verdict);
        assert_eq!(parsed.outcome(), Outcome::Ask);
        assert_eq!(parsed.rule(), rule);
        assert_eq!(parsed.reason(), reason);
    }

    #[test]
    fn refuses_an_empty_rule_during_deserialization() {
        let json = json!({ "outcome": "ask", "rule": "", "reason": "reason" });
        let error = serde_json::from_value::<Verdict>(json).unwrap_err();

        assert_eq!(error.to_string(), VerdictError::EmptyRule.to_string());
    }

    #[test]
    fn refuses_an_unknown_field_during_deserialization() {
        let json = json!({
            "outcome": "ask",
            "rule": "shell-guard",
            "reason": "reason",
            "extra": 1,
        });
        let error = serde_json::from_value::<Verdict>(json).unwrap_err();

        assert_eq!(
            error.to_string(),
            "unknown field `extra`, expected one of `outcome`, `rule`, `reason`"
        );
    }
}
