//! Chauffeur's judgment core: rules judge a bounded request and the
//! strictest opinion becomes the verdict.
//!
//! A request arrives as JSON, is parsed and validated once, and the engine
//! asks each rule in order. The engine keeps the strictest outcome and a
//! trace of what each rule said.
//!
//! ```
//! use chauffeur_judgment::{Engine, Outcome, Request, RequestError, RuleSet};
//!
//! let request = Request::parse(
//!     r#"{
//!         "session": "s1",
//!         "hook": "tool_after",
//!         "subject": { "tool": "shell", "input": { "command": "git status" } }
//!     }"#,
//! )?;
//! let judgment = Engine::new(RuleSet::builtin()).judge(&request);
//!
//! assert_eq!(judgment.verdict.outcome, Outcome::Allow);
//! # Ok::<(), RequestError>(())
//! ```

mod engine;
mod request;
mod rule;
mod rules;
mod trace;
mod verdict;

pub use engine::{Engine, Judgment};
pub use request::{DEFAULT_DEADLINE_MS, MAX_DEADLINE_MS, MAX_REQUEST_BYTES};
pub use request::{MAX_EVIDENCE_BLOCKS, MAX_EVIDENCE_BYTES, MAX_TEXT_BYTES};
pub use request::{Request, RequestError};
pub use rule::{MAX_RULES, Rule, RuleSet, RuleSetError};
pub use rules::ShellGuard;
pub use trace::{MAX_EVENTS, Trace, TraceEvent};
pub use verdict::{Outcome, Verdict};
