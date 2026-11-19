//! Runs a rule for an event, or a navigation's strategy: admits it within a
//! capacity, redacts the evidence and asks the judges on a bounded pool of
//! worker threads, until the deadline or the caller's cancellation.

mod admission;
#[cfg(kani)]
mod proofs;
mod redact;
mod run;

pub use admission::{Admission, Busy, Capacity, Ledger, Limit, Permit, Source};
pub use redact::{Masking, Prefix, REDACTED, RedactError, RedactionConfig, Redactor};
pub use run::{
    Cancel, Context, EventLog, ExecuteError, Job, Keys, MAX_EVENTS, Pool, Ran, Rests, Waited, run,
    run_strategy, within,
};
