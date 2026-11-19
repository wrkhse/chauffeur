//! The System One tier: fast classifiers. Its `systemone` format speaks the
//! `/v1/systemone` API that Jev and other classifiers serve.

#[cfg(feature = "systemone")]
mod client;
#[cfg(feature = "systemone")]
mod wire;

#[cfg(feature = "systemone")]
pub use client::SystemOne;
use serde::Deserialize;

use crate::config::KeySource;
use crate::format::Format;

/// How a model is asked a question whose outcomes are not exactly `yes` and
/// `no`. A yes/no question is always asked as `noul`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemOneAsk {
    /// As `choice`: the outcomes are options, each with its description.
    #[default]
    Choice,
    /// As `score`: the outcomes are a scale's levels, lowest first.
    Score,
}

/// A model that speaks the `/v1/systemone` API.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemOneEntry {
    /// The API's base URL; requests go to `{endpoint}/v1/systemone`.
    pub endpoint: String,
    /// The model ID sent with each request.
    pub model: String,
    /// The key, sent as a bearer token.
    pub key: KeySource,
    /// How long one call may take, in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Sent as the request's top-level `store` when set, and left out when
    /// not: a provider may refuse a top-level key it does not know.
    pub store: Option<bool>,
    /// How a question that is not yes/no is asked.
    #[serde(default)]
    pub ask: SystemOneAsk,
}

/// A client for `entry` that sends `key`.
#[cfg(feature = "systemone")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the build without the `systemone` feature"
)]
pub fn format(entry: &SystemOneEntry, key: String) -> Option<Box<dyn Format>> {
    Some(Box::new(SystemOne::new(entry, key)))
}

/// Built without HTTP, as for the browser: the model is left out.
#[cfg(not(feature = "systemone"))]
pub fn format(_: &SystemOneEntry, _: String) -> Option<Box<dyn Format>> {
    None
}
