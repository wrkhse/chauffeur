use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a model rests after it cannot answer.
pub const REST: Duration = Duration::from_secs(60);

/// The models resting now, shared by every tier in a process.
///
/// A model that could not answer rests for `REST` and is skipped while it
/// rests; an answer ends its rest. The clock is read only for a model that
/// has an entry, so a fresh registry, like a round with no failure, reads no
/// clock.
#[derive(Clone)]
pub struct Rests {
    entries: Arc<Mutex<BTreeMap<String, Instant>>>,
    now: fn() -> Instant,
    bound: usize,
}

impl Rests {
    /// A fresh registry, holding at most 4,096 resting models.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Instant::now, 4_096)
    }

    /// A fresh registry that reads `now` and holds at most `bound` models.
    /// `now` is the seam for a clock advanced without a sleep in a test.
    #[must_use]
    pub fn with_clock(now: fn() -> Instant, bound: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            now,
            bound,
        }
    }

    /// Whether `name` is still resting. With no entry the clock is not read.
    /// A registry that cannot be locked does not rest the model.
    #[must_use]
    pub fn cooling(&self, name: &str) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        entries.get(name).is_some_and(|until| (self.now)() < *until)
    }

    /// Rests `name` for `REST`, forgetting the model whose rest ends first
    /// once the registry is at its bound. A registry that cannot be locked,
    /// or a clock that cannot move `REST` ahead, leaves the model unrested.
    pub fn rest(&self, name: &str) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let Some(until) = (self.now)().checked_add(REST) else {
            return;
        };
        if entries.len() >= self.bound
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(name, _)| name.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(name.into(), until);
    }

    /// Ends `name`'s rest, so a model that answered is asked again at once.
    pub fn clear(&self, name: &str) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.remove(name);
    }

    /// Forgets every rest, so a test can start with no model resting.
    pub fn reset(&self) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.clear();
    }

    /// How many models are resting.
    #[must_use]
    pub fn len(&self) -> usize {
        let Ok(entries) = self.entries.lock() else {
            return 0;
        };
        entries.len()
    }

    /// Whether no model is resting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return true;
        };
        entries.is_empty()
    }
}

impl Default for Rests {
    fn default() -> Self {
        Self::new()
    }
}
