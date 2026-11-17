//! A rule file as written, before its checks.
//!
//! Only `rule` and its modules read these files, so its types are
//! `pub(super)` except the three `rule` re-exports; the module is private, so
//! `pub(crate)` would be redundant.

use std::collections::BTreeMap;

use chauffeur_judges::Tier;
use serde::Deserialize;
use serde_json::Value;

use crate::when::When;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuleFile {
    pub(super) on: String,
    #[serde(default)]
    pub(super) when: When,
    #[serde(default)]
    pub(super) judges: BTreeMap<String, JudgeFile>,
    #[serde(default)]
    pub(super) asks: BTreeMap<String, AskFile>,
    pub(super) strategy: Vec<StepFile>,
    #[serde(default)]
    pub(super) then: Vec<ThenFile>,
    pub(super) cooldown_seconds: Option<u64>,
}

/// A named judge as written, before its confidence and model are checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeFile {
    pub(super) tier: Tier,
    pub(super) model: Option<String>,
    pub(super) confidence: Option<ConfidenceFile>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ConfidenceFile {
    Bar(f64),
    PerOutcome(BTreeMap<String, f64>),
}

/// A question as written, inline in a step or named in `asks`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskFile {
    pub(super) instructions: String,
    pub(super) outcomes: Vec<LabelFile>,
    #[serde(default)]
    pub(super) evidence: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum LabelFile {
    Value(String),
    Described(DescribedFile),
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DescribedFile {
    pub(super) value: String,
    pub(super) description: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum AskRef {
    Named(String),
    Inline(AskFile),
}

/// One step as written: its ID, its judge, and its ask.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepFile {
    pub(super) id: String,
    pub(super) judge: String,
    pub(super) ask: AskRef,
}

#[derive(Deserialize)]
pub(super) struct ThenFile {
    #[serde(rename = "if", default)]
    pub(super) needs: BTreeMap<String, String>,
    #[serde(flatten)]
    pub(super) does: BTreeMap<String, Value>,
}
