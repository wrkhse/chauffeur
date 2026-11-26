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
//! let verdict = judgment.verdict().expect("the shell guard allows git status");
//!
//! assert_eq!(verdict.outcome(), Outcome::Allow);
//! # Ok::<(), RequestError>(())
//! ```

mod audit;
mod engine;
mod request;
mod rule;
mod rules;
mod trace;
mod verdict;

pub use audit::{
    AuditError, AuditLog, AuditRecord, MAX_AUDIT_BYTES, MAX_RECENT, MAX_RECORD_BYTES,
};
pub use engine::{Engine, Judgment, JudgmentError};
pub use request::{MAX_EVIDENCE_BLOCKS, MAX_EVIDENCE_BYTES, MAX_TEXT_BYTES};
pub use request::{MAX_REQUEST_BYTES, Request, RequestError};
pub use rule::{MAX_RULES, Rule, RuleSet, RuleSetError};
pub use rules::ShellGuard;
pub use trace::{MAX_EVENTS, Trace, TraceError, TraceEvent};
pub use verdict::{MAX_REASON_BYTES, MAX_RULE_NAME_BYTES};
pub use verdict::{Outcome, Verdict, VerdictError};
