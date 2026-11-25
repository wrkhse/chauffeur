//! The judgment rule interface and its bounded, ordered registry.

use std::fmt;

use crate::rules::ShellGuard;
use crate::verdict::MAX_RULE_NAME_BYTES;
use crate::{Request, Trace, Verdict};

/// The maximum number of rules in a set.
pub const MAX_RULES: usize = 16;

/// A named rule that can give an opinion on a request.
pub trait Rule: Send + Sync {
    /// Returns the rule's static name.
    fn name(&self) -> &'static str;

    /// Gives an opinion and records one matching opinion event, or abstains.
    fn judge(&self, request: &Request, trace: &mut Trace) -> Option<Verdict>;
}

/// Why a rule cannot join a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleSetError {
    /// The rule's name is empty.
    EmptyName,
    /// The rule's name is longer than `MAX_RULE_NAME_BYTES`.
    TooLong,
    /// The set already holds a rule with this name.
    Duplicate,
    /// The set already holds the maximum number of rules.
    Full,
}

impl fmt::Display for RuleSetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("rule name is empty"),
            Self::TooLong => {
                write!(formatter, "rule name exceeds {MAX_RULE_NAME_BYTES} bytes")
            }
            Self::Duplicate => {
                formatter.write_str("rule name is already in the set")
            }
            Self::Full => formatter.write_str("rule set is full"),
        }
    }
}

impl std::error::Error for RuleSetError {}

/// At most `MAX_RULES` distinct rules in insertion order.
#[derive(Default)]
pub struct RuleSet {
    rules: Vec<Box<dyn Rule>>,
}

impl RuleSet {
    /// Creates an empty rule set.
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Adds a rule at the end of the set.
    ///
    /// # Errors
    ///
    /// Returns `EmptyName` for an empty name, `TooLong` for a name over
    /// `MAX_RULE_NAME_BYTES`, `Duplicate` for a name already in the set, or
    /// `Full` when the set holds `MAX_RULES` rules. Checks run in that order.
    pub fn insert(&mut self, rule: Box<dyn Rule>) -> Result<(), RuleSetError> {
        let name = rule.name();

        if name.is_empty() {
            return Err(RuleSetError::EmptyName);
        }
        if name.len() > MAX_RULE_NAME_BYTES {
            return Err(RuleSetError::TooLong);
        }
        if self.iter().any(|existing| existing.name() == name) {
            return Err(RuleSetError::Duplicate);
        }
        if self.rules.len() >= MAX_RULES {
            return Err(RuleSetError::Full);
        }

        self.rules.push(rule);
        Ok(())
    }

    /// Visits rules in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Rule> {
        self.rules.iter().map(Box::as_ref)
    }

    /// Returns rule names in insertion order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.iter().map(Rule::name).collect()
    }

    /// Creates the built-in rule set.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            rules: vec![Box::new(ShellGuard)],
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Request, Trace, Verdict};

    use super::{MAX_RULE_NAME_BYTES, MAX_RULES, Rule, RuleSet, RuleSetError};

    const LONGEST_NAME: &str = concat!(
        "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh",
        "abcdefgh", "abcdefgh",
    );
    const TOO_LONG_NAME: &str = concat!(
        "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh", "abcdefgh",
        "abcdefgh", "abcdefgh", "x",
    );
    const NAMES: [&str; MAX_RULES] = [
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13",
        "14", "15",
    ];

    struct NamedRule(&'static str);

    impl Rule for NamedRule {
        fn name(&self) -> &'static str {
            self.0
        }

        fn judge(&self, _: &Request, _: &mut Trace) -> Option<Verdict> {
            None
        }
    }

    #[test]
    fn lists_rules_in_insertion_order() {
        let mut rules = RuleSet::new();

        assert_eq!(rules.insert(Box::new(NamedRule("z"))), Ok(()));
        assert_eq!(rules.insert(Box::new(NamedRule("a"))), Ok(()));
        assert_eq!(rules.names(), vec!["z", "a"]);
    }

    #[test]
    fn refuses_duplicate_names() {
        let mut rules = RuleSet::new();

        assert_eq!(rules.insert(Box::new(NamedRule("same"))), Ok(()));
        assert_eq!(
            rules.insert(Box::new(NamedRule("same"))),
            Err(RuleSetError::Duplicate)
        );
        assert_eq!(rules.names(), vec!["same"]);
    }

    #[test]
    fn refuses_an_empty_name() {
        let mut rules = RuleSet::new();

        assert_eq!(
            rules.insert(Box::new(NamedRule(""))),
            Err(RuleSetError::EmptyName)
        );
        assert_eq!(rules.names(), Vec::<&str>::new());
    }

    #[test]
    fn accepts_a_name_at_the_byte_limit_and_refuses_one_past_it() {
        let mut rules = RuleSet::new();

        assert_eq!(LONGEST_NAME.len(), MAX_RULE_NAME_BYTES);
        assert_eq!(rules.insert(Box::new(NamedRule(LONGEST_NAME))), Ok(()));
        assert_eq!(
            rules.insert(Box::new(NamedRule(TOO_LONG_NAME))),
            Err(RuleSetError::TooLong)
        );
        assert_eq!(rules.names(), vec![LONGEST_NAME]);
    }

    #[test]
    fn reports_an_invalid_or_duplicate_name_before_a_full_set() {
        let mut rules = RuleSet::new();

        for name in NAMES {
            assert_eq!(rules.insert(Box::new(NamedRule(name))), Ok(()));
        }

        assert_eq!(
            rules.insert(Box::new(NamedRule(""))),
            Err(RuleSetError::EmptyName)
        );
        assert_eq!(
            rules.insert(Box::new(NamedRule(TOO_LONG_NAME))),
            Err(RuleSetError::TooLong)
        );
        assert_eq!(
            rules.insert(Box::new(NamedRule("0"))),
            Err(RuleSetError::Duplicate)
        );
        assert_eq!(rules.names(), NAMES);
    }

    #[test]
    fn refuses_rules_past_the_limit() {
        let mut rules = RuleSet::new();

        for name in NAMES {
            assert_eq!(rules.insert(Box::new(NamedRule(name))), Ok(()));
        }

        assert_eq!(
            rules.insert(Box::new(NamedRule("extra"))),
            Err(RuleSetError::Full)
        );
        assert_eq!(rules.names(), NAMES);
    }

    #[test]
    fn displays_rule_set_errors() {
        assert_eq!(RuleSetError::EmptyName.to_string(), "rule name is empty");
        assert_eq!(
            RuleSetError::TooLong.to_string(),
            "rule name exceeds 64 bytes"
        );
        assert_eq!(RuleSetError::Full.to_string(), "rule set is full");
        assert_eq!(
            RuleSetError::Duplicate.to_string(),
            "rule name is already in the set"
        );
    }

    #[test]
    fn builtin_contains_the_shell_guard() {
        assert_eq!(RuleSet::builtin().names(), vec!["shell-guard"]);
    }
}
