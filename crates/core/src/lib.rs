//! Chauffeur's core: a plan of named steps, each asked of a named judge once
//! the earlier steps it waits on allow it, and the events and verdict of
//! running it. Its one dependency is rungs, the step machine; it knows
//! nothing of models, transports or harnesses.
//!
//! ```
//! use chauffeur_core::{Outcome, Panel, Plan, Reply, Request, Step};
//!
//! /// A yes/no question.
//! struct Ask(&'static str);
//!
//! impl Request for Ask {
//!     fn outcomes(&self) -> u8 {
//!         2
//!     }
//! }
//!
//! /// An answer: 0 for yes, 1 for no.
//! #[derive(Clone, Debug, PartialEq)]
//! struct Said(u8);
//!
//! impl Reply for Said {
//!     fn settled(&self) -> u8 {
//!         self.0
//!     }
//!
//!     fn p(&self, outcome: u8) -> Option<u16> {
//!         (outcome < 2).then_some(if outcome == self.0 { 1000 } else { 0 })
//!     }
//! }
//!
//! let plan = Plan::new([
//!     Step::new("commit", "system-one", Ask("Is this message conventional?")),
//!     Step::new("rule", "facts", Ask("Does the diff touch migrations?")),
//! ])?;
//! let yes = |_: &Ask| Outcome::Value(Said(0));
//! let no = |_: &Ask| Outcome::Value(Said(1));
//! let panel = Panel::new().with("system-one", &yes).with("facts", &no);
//!
//! let run = plan.run(&panel)?;
//! assert_eq!(run.verdict.get("commit"), Some(&Outcome::Value(Said(0))));
//! assert_eq!(run.verdict.get("rule"), Some(&Outcome::Value(Said(1))));
//! # Ok::<(), chauffeur_core::PlanError>(())
//! ```

mod event;
mod judge;
mod plan;
#[cfg(kani)]
mod proofs;

pub use event::{Event, Run, Verdict};
pub use judge::{Judge, Panel};
pub use plan::{
    Bar, Failure, MAX_STEPS, MIN_ESCALATION, Near, Need, Outcome, Plan, PlanError, RESERVE, Reply,
    ReplyError, Request, Round, Running, Step, Wants, When,
};
