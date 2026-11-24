//! The bounded record of how an engine reaches its verdict.

use std::fmt;

use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Outcome;

/// The maximum number of events a trace keeps.
pub const MAX_EVENTS: usize = 256;

/// An event in the order it occurs during judgment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// The engine calls a rule.
    RuleStarted { rule: String },
    /// A rule gives an opinion.
    RuleOpinion {
        rule: String,
        outcome: Outcome,
        reason: String,
    },
    /// The engine chooses the final verdict.
    Decided { rule: String, outcome: Outcome },
}

/// The events retained in order and the count of events dropped at the limit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    #[serde(deserialize_with = "deserialize_events")]
    events: Vec<TraceEvent>,
    dropped: usize,
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

    /// Keeps an event if there is room; otherwise counts it and returns false.
    pub fn push(&mut self, event: TraceEvent) -> bool {
        // Bounded append adapts origin/v2 crates/runtime/src/run/log.rs
        // at 55bca73.
        if self.events.len() >= MAX_EVENTS {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }

        self.events.push(event);
        true
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
    use super::{MAX_EVENTS, Trace, TraceEvent};

    #[test]
    fn keeps_the_first_events_in_order_and_counts_overflow() {
        let mut trace = Trace::new();
        let events: Vec<_> = (0..MAX_EVENTS)
            .map(|index| TraceEvent::RuleStarted {
                rule: index.to_string(),
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

        assert_eq!(trace.events(), events.as_slice());
    }

    #[test]
    fn serializes_a_tagged_event_and_explicit_dropped_count() {
        let mut trace = Trace::new();
        trace.push(started());
        let json = concat!(
            r#"{"events":[{"kind":"rule_started","rule":"shell-guard"}],"#,
            r#""dropped":0}"#,
        );

        assert_eq!(serde_json::to_string(&trace).unwrap(), json);
        assert_eq!(serde_json::from_str::<Trace>(json).unwrap(), trace);
    }

    #[test]
    fn repeated_overflow_saturates_without_replacing_events() {
        let events = vec![started(); MAX_EVENTS];
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
        let json = r#"{"events":7,"dropped":0}"#;
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
        let json = serde_json::json!({ "events": events, "dropped": 0 });

        let error = serde_json::from_value::<Trace>(json).unwrap_err();

        assert_eq!(error.to_string(), "too many trace events");
    }

    fn started() -> TraceEvent {
        TraceEvent::RuleStarted {
            rule: "shell-guard".to_owned(),
        }
    }
}
