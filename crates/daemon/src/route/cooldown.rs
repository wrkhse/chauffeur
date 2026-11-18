use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many project, rule and session triples keep a cooldown.
const BOUND: usize = 4_096;

/// The project whose rule fired, when it was a project's rule, the rule's ID,
/// and the session it returned something in. A project's rule keeps its own
/// cooldown, so two projects with a rule of the same ID do not suppress each
/// other.
pub(super) type Key = (Option<PathBuf>, String, String);

/// When each rule last returned something in each session. Past its bound,
/// the oldest triple is forgotten.
///
/// [`Cooldowns::cooling`] spares a run its judges while a triple cools down,
/// and [`Cooldowns::try_fire`] decides, under one lock, which of the runs
/// that settle at once returns its action.
pub(super) struct Cooldowns {
    fired: Mutex<BTreeMap<Key, Instant>>,
}

impl Cooldowns {
    pub(super) const fn new() -> Self {
        Self {
            fired: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether `key` returned something within `cooldown`.
    pub(super) fn cooling(&self, key: &Key, cooldown: Option<Duration>) -> bool {
        let (Some(cooldown), Ok(fired)) = (cooldown, self.fired.lock()) else {
            return false;
        };
        fired.get(key).is_some_and(|at| at.elapsed() < cooldown)
    }

    /// Records that `key` returns something now, unless another run did
    /// within `cooldown`. A rule with no cooldown always returns.
    pub(super) fn try_fire(&self, key: Key, cooldown: Option<Duration>) -> bool {
        let Some(cooldown) = cooldown else {
            return true;
        };
        let Ok(mut fired) = self.fired.lock() else {
            return true;
        };
        if fired.get(&key).is_some_and(|at| at.elapsed() < cooldown) {
            return false;
        }
        if fired.len() >= BOUND
            && let Some(oldest) = fired
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone())
        {
            fired.remove(&oldest);
        }
        fired.insert(key, Instant::now());
        true
    }
}
