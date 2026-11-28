//! The bounded record of how an engine reaches its verdict.

use std::fmt;

use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Outcome;
use crate::verdict::{MAX_REASON_BYTES, MAX_RULE_NAME_BYTES};

/// The maximum number of events a trace keeps.
pub const MAX_EVENTS: usize = 256;

/// The rule events a trace keeps before the engine's reserved final slot.
const MAX_RULE_EVENTS: usize = MAX_EVENTS - 1;

/// An event in the order it occurs during judgment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "EventKind", into = "EventKind")]
pub struct TraceEvent(EventKind);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EventKind {
    RuleStarted {
        rule: String,
    },
    RuleOpinion {
        rule: String,
        outcome: Outcome,
        reason: String,
    },
    Decided {
        rule: String,
        outcome: Outcome,
    },
    Finished,
}

/// Why an event cannot join a trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceError {
    /// The event's rule name is empty.
    EmptyRule,
    /// A field exceeds its byte limit.
    Limit {
        /// The field that exceeds the limit.
        name: &'static str,
        /// The field's byte limit.
        max: u64,
    },
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRule => f.write_str("rule is empty"),
            Self::Limit { name, max } => write!(f, "{name} exceeds {max}"),
        }
    }
}

impl std::error::Error for TraceError {}

impl TraceEvent {
    const FINISHED: Self = Self(EventKind::Finished);

    /// Records that the engine calls a rule.
    ///
    /// # Errors
    /// Returns an error if the rule name is empty or too long.
    pub fn rule_started(rule: &str) -> Result<Self, TraceError> {
        check_rule(rule)?;

        Ok(Self(EventKind::RuleStarted {
            rule: rule.to_owned(),
        }))
    }

    /// Records that a rule gives an opinion.
    ///
    /// # Errors
    /// Returns an error if the rule name is empty or either field is too long.
    pub fn rule_opinion(
        rule: &str,
        outcome: Outcome,
        reason: &str,
    ) -> Result<Self, TraceError> {
        check_rule(rule)?;
        check_len("reason", reason, MAX_REASON_BYTES)?;

        Ok(Self(EventKind::RuleOpinion {
            rule: rule.to_owned(),
            outcome,
            reason: reason.to_owned(),
        }))
    }

    /// Records the verdict the engine chooses.
    ///
    /// # Errors
    /// Returns an error if the rule name is empty or too long.
    pub fn decided(rule: &str, outcome: Outcome) -> Result<Self, TraceError> {
        check_rule(rule)?;

        Ok(Self(EventKind::Decided {
            rule: rule.to_owned(),
            outcome,
        }))
    }

    /// Records that the engine finishes, with or without a verdict.
    ///
    /// # Errors
    /// This event has no fields to validate and its construction cannot fail.
    pub const fn finished() -> Result<Self, TraceError> {
        Ok(Self::FINISHED)
    }

    const fn is_finished(&self) -> bool {
        matches!(self.0, EventKind::Finished)
    }
}

impl From<TraceEvent> for EventKind {
    fn from(event: TraceEvent) -> Self {
        event.0
    }
}

impl TryFrom<EventKind> for TraceEvent {
    type Error = TraceError;

    fn try_from(event: EventKind) -> Result<Self, Self::Error> {
        match &event {
            EventKind::RuleStarted { rule } | EventKind::Decided { rule, .. } => {
                check_rule(rule)?;
            }
            EventKind::RuleOpinion { rule, reason, .. } => {
                check_rule(rule)?;
                check_len("reason", reason, MAX_REASON_BYTES)?;
            }
            EventKind::Finished => {}
        }

        Ok(Self(event))
    }
}

fn check_rule(rule: &str) -> Result<(), TraceError> {
    if rule.is_empty() {
        return Err(TraceError::EmptyRule);
    }

    check_len("rule", rule, MAX_RULE_NAME_BYTES)
}

fn check_len(name: &'static str, text: &str, max: usize) -> Result<(), TraceError> {
    if text.len() > max {
        return Err(TraceError::Limit {
            name,
            max: u64::try_from(max).unwrap_or(u64::MAX),
        });
    }

    Ok(())
}

/// The events retained in order and the count of events dropped at the limit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TraceFile")]
pub struct Trace {
    events: Vec<TraceEvent>,
    // A runtime bound diagnostic, deliberately outside the persisted trace
    // format: the strict reader still refuses a supplied count.
    #[serde(skip_serializing)]
    dropped: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceFile {
    #[serde(deserialize_with = "deserialize_events")]
    events: Vec<TraceEvent>,
}

impl TryFrom<TraceFile> for Trace {
    type Error = &'static str;

    fn try_from(file: TraceFile) -> Result<Self, Self::Error> {
        let mut events = file.events.as_slice();

        if let Some((last, rest)) = events.split_last()
            && last.is_finished()
        {
            events = rest;
        }

        if events.len() > MAX_RULE_EVENTS {
            return Err("the final trace slot is reserved for finished");
        }
        if events.iter().any(TraceEvent::is_finished) {
            return Err("finished must be the last trace event");
        }

        Ok(Self {
            events: file.events,
            dropped: 0,
        })
    }
}

impl Trace {
    /// Starts an empty trace.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: Vec::new(),
            dropped: 0,
        }
    }

    /// Keeps a non-final event if there is room before the reserved slot.
    /// Counts and returns `false` for an event it cannot keep.
    pub fn push(&mut self, event: TraceEvent) -> bool {
        // Bounded append adapts origin/v2 crates/runtime/src/run/log.rs
        // at 55bca73.
        let full = self.events.len() >= MAX_RULE_EVENTS;

        if full || self.is_finished() || event.is_finished() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }

        self.events.push(event);
        true
    }

    /// Records the final event once, even when no rule events fit.
    pub(crate) fn finish(&mut self) {
        if !self.is_finished() {
            self.events.push(TraceEvent::FINISHED);
        }
    }

    fn is_finished(&self) -> bool {
        self.events.last().is_some_and(TraceEvent::is_finished)
    }

    /// Returns the retained events in order.
    #[must_use]
    pub const fn events(&self) -> &[TraceEvent] {
        self.events.as_slice()
    }

    /// Returns the saturating count of events dropped at the limit.
    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }
}

fn deserialize_events<'de, D>(deserializer: D) -> Result<Vec<TraceEvent>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedEvents;

    impl<'de> Visitor<'de> for BoundedEvents {
        type Value = Vec<TraceEvent>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_EVENTS} trace events")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut events = Vec::new();

            for _ in 0..MAX_EVENTS {
                let Some(event) = sequence.next_element()? else {
                    return Ok(events);
                };

                events.push(event);
            }

            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("too many trace events"));
            }

            Ok(events)
        }
    }

    deserializer.deserialize_seq(BoundedEvents)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MAX_EVENTS, MAX_REASON_BYTES, MAX_RULE_NAME_BYTES, Outcome, Trace,
        TraceError, TraceEvent,
    };

    #[test]
    fn reserves_the_last_slot_for_finished() {
        let mut trace = Trace::new();
        let mut events: Vec<_> = (0..255)
            .map(|index| {
                TraceEvent::rule_started(&index.to_string()).expect("valid event")
            })
            .collect();

        for event in &events {
            let kept = trace.push(event.clone());
            assert_eq!((kept, trace.dropped()), (true, 0));
        }

        for dropped in 1..=2 {
            let kept = trace.push(started());
            assert_eq!((kept, trace.dropped()), (false, dropped));
        }

        trace.finish();
        trace.finish();
        events.push(TraceEvent::finished().expect("valid event"));

        assert_eq!(trace.events().len(), 256);
        assert_eq!(trace.events(), events.as_slice());
        assert_eq!(trace.dropped(), 2);
    }

    #[test]
    fn round_trips_the_serialized_trace() {
        let mut trace = Trace::new();
        trace.push(started());
        trace.push(
            TraceEvent::rule_opinion("shell-guard", Outcome::Ask, "shell path")
                .expect("valid event"),
        );
        trace.push(
            TraceEvent::decided("shell-guard", Outcome::Ask).expect("valid event"),
        );
        trace.finish();

        let expected = concat!(
            r#"{"events":[{"kind":"rule_started","rule":"shell-guard"},"#,
            r#"{"kind":"rule_opinion","rule":"shell-guard","outcome":"ask","#,
            r#""reason":"shell path"},"#,
            r#"{"kind":"decided","rule":"shell-guard","outcome":"ask"},"#,
            r#"{"kind":"finished"}]}"#,
        );

        let json = serde_json::to_string(&trace).unwrap();

        assert_eq!(json, expected);
        assert_eq!(serde_json::from_str::<Trace>(&json).unwrap(), trace);
    }

    #[test]
    fn does_not_serialize_the_runtime_dropped_count() {
        let mut trace = Trace::new();
        let kept = trace.push(TraceEvent::finished().expect("valid event"));

        assert_eq!((kept, trace.dropped()), (false, 1));
        trace.finish();

        let json = serde_json::to_value(&trace).unwrap();

        assert_eq!(json, json!({ "events": [{ "kind": "finished" }] }));

        let read_back = serde_json::from_value::<Trace>(json).unwrap();

        assert_eq!(read_back.events(), trace.events());
    }

    #[test]
    fn deserialization_refuses_a_supplied_dropped_count() {
        let error = serde_json::from_value::<Trace>(json!({
            "events": [], "dropped": 0,
        }))
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "unknown field `dropped`, expected `events`"
        );
    }

    #[test]
    fn repeated_overflow_saturates_without_replacing_events() {
        let events = vec![started(); MAX_EVENTS - 1];
        let mut trace = Trace {
            events: events.clone(),
            dropped: usize::MAX.saturating_sub(1),
        };

        for _ in 0..2 {
            let kept = trace.push(started());
            assert_eq!((kept, trace.dropped()), (false, usize::MAX));
        }

        assert_eq!(trace.events(), events.as_slice());
    }

    #[test]
    fn deserialization_rejects_non_array_events() {
        let json = r#"{"events":7}"#;
        let error = serde_json::from_str::<Trace>(json).unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid type: integer `7`, expected at most 256 trace events \
             at line 1 column 11"
        );
    }

    #[test]
    fn deserialization_rejects_more_than_the_event_limit() {
        let mut events = vec![started(); MAX_EVENTS];
        events.push(started());
        let json = json!({ "events": events });

        let error = serde_json::from_value::<Trace>(json).unwrap_err();

        assert_eq!(error.to_string(), "too many trace events");
    }

    #[test]
    fn deserialization_reserves_the_last_slot_for_finished() {
        let mut events = vec![started(); MAX_EVENTS];
        let file = json!({ "events": events });
        let error = serde_json::from_value::<Trace>(file).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the final trace slot is reserved for finished"
        );

        events[MAX_EVENTS - 1] = TraceEvent::finished().expect("valid event");
        let file = json!({ "events": events });
        let trace = serde_json::from_value::<Trace>(file).unwrap();

        assert_eq!(trace.events(), events.as_slice());
    }

    #[test]
    fn refuses_rule_events_after_finished() {
        let finished = TraceEvent::finished().expect("valid event");
        let mut trace = Trace::new();
        let kept = trace.push(finished.clone());

        assert_eq!((kept, trace.dropped()), (false, 1));

        trace.finish();
        let kept = trace.push(started());

        assert_eq!((kept, trace.dropped()), (false, 2));
        assert_eq!(trace.events(), std::slice::from_ref(&finished));

        let file = json!({ "events": [finished, started()] });
        let error = serde_json::from_value::<Trace>(file).unwrap_err();

        assert_eq!(error.to_string(), "finished must be the last trace event");
    }

    #[test]
    fn refuses_invalid_rule_and_reason_text_at_both_boundaries() {
        let rule = "é".repeat(33);
        let reason = "é".repeat(129);

        let rule_limit = TraceError::Limit {
            name: "rule",
            max: 64,
        };
        assert_eq!(TraceEvent::rule_started(&rule), Err(rule_limit));
        assert_eq!(TraceEvent::decided(&rule, Outcome::Allow), Err(rule_limit));
        assert_eq!(TraceEvent::rule_started(""), Err(TraceError::EmptyRule));
        assert_eq!(
            TraceEvent::rule_opinion("rule", Outcome::Ask, &reason),
            Err(TraceError::Limit {
                name: "reason",
                max: 256
            }),
        );

        let files = [
            (
                json!({ "kind": "rule_started", "rule": rule }),
                "rule exceeds 64",
            ),
            (
                json!({ "kind": "rule_started", "rule": "" }),
                "rule is empty",
            ),
            (
                json!({
                    "kind": "rule_opinion", "rule": "rule", "outcome": "ask",
                    "reason": reason,
                }),
                "reason exceeds 256",
            ),
        ];

        for (event, message) in files {
            let file = json!({ "events": [event] });
            let error = serde_json::from_value::<Trace>(file).unwrap_err();

            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn accepts_rule_and_reason_text_at_the_byte_limits() {
        let rule = "r".repeat(MAX_RULE_NAME_BYTES);
        let reason = "e".repeat(MAX_REASON_BYTES);
        let started = TraceEvent::rule_started(&rule).unwrap();
        let opinion =
            TraceEvent::rule_opinion(&rule, Outcome::Ask, &reason).unwrap();

        assert_eq!(
            serde_json::to_value(started).unwrap(),
            json!({ "kind": "rule_started", "rule": rule })
        );
        assert_eq!(
            serde_json::to_value(opinion).unwrap(),
            json!({
                "kind": "rule_opinion",
                "rule": rule,
                "outcome": "ask",
                "reason": reason,
            })
        );
    }

    #[test]
    fn refuses_rule_and_reason_text_one_byte_over_the_limits() {
        let rule = "r".repeat(MAX_RULE_NAME_BYTES + 1);
        let reason = "e".repeat(MAX_REASON_BYTES + 1);
        let rule_limit = TraceError::Limit {
            name: "rule",
            max: 64,
        };
        let reason_limit = TraceError::Limit {
            name: "reason",
            max: 256,
        };

        assert_eq!(TraceEvent::rule_started(&rule), Err(rule_limit));
        assert_eq!(
            TraceEvent::rule_opinion(&rule, Outcome::Ask, "reason"),
            Err(rule_limit)
        );
        assert_eq!(
            TraceEvent::rule_opinion("rule", Outcome::Ask, &reason),
            Err(reason_limit)
        );
    }

    fn started() -> TraceEvent {
        TraceEvent::rule_started("shell-guard").expect("valid event")
    }
}
