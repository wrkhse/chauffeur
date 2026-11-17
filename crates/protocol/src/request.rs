use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chauffeur_judges::Tier;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Map, Value, json};

use crate::event::Event;
use crate::limits::{Exceeded, Limit, count};
use crate::rule::{AskFile, JudgeFile, RuleError, StepFile, Strategy};

/// The protocol versions the daemon accepts.
pub const PROTOCOLS: [u64; 1] = [1];

/// How long rules may take when a request names no deadline.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(3);

/// The ID an inline strategy's problems are reported under.
const INLINE: &str = "inline";

/// JSON-RPC's code for invalid params, and the one this daemon uses for a
/// request it could not serve.
const INVALID_PARAMS: i64 = -32_602;
const SERVER_ERROR: i64 = -32_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    // Checked before the rest is read.
    #[serde(rename = "protocol")]
    _protocol: IgnoredAny,
    deadline_ms: Option<u64>,
    on: Option<String>,
    project: Option<String>,
    strategy: Option<Vec<StepFile>>,
    judges: Option<BTreeMap<String, JudgeFile>>,
    asks: Option<BTreeMap<String, AskFile>>,
    #[serde(default)]
    evidence: BTreeMap<String, String>,
    #[serde(default)]
    candidates: BTreeMap<String, String>,
}

/// One `chauffeur.navigate` request, checked: its protocol, its limits and,
/// for an inline strategy, the strategy itself.
#[derive(Debug)]
pub struct Navigate {
    deadline: Duration,
    project: Option<PathBuf>,
    evidence: Vec<(String, String)>,
    candidates: Vec<(String, String)>,
    target: Target,
}

/// What a request asks: the rules hooking an event, or one inline strategy.
#[derive(Debug)]
pub enum Target {
    /// Every rule whose `on` and `when` match answers with its intents.
    On(String),
    /// Only this strategy runs, and its verdict returns.
    Strategy(Strategy),
}

impl Navigate {
    /// Checks a request's `params`, in order: the protocol, the fields, the
    /// limits, then an inline strategy.
    ///
    /// # Errors
    ///
    /// A [`Rejection`]: `unsupported_protocol`, `limit_exceeded` or `invalid`.
    pub fn parse(params: &Value) -> Result<Self, Rejection> {
        let protocol = params
            .get("protocol")
            .ok_or_else(|| Rejection::Invalid("a request needs `protocol`".to_owned()))?;
        let Some(version) = protocol.as_u64() else {
            return Err(Rejection::Invalid(
                "`protocol` is a whole number, such as 1".to_owned(),
            ));
        };
        if !PROTOCOLS.contains(&version) {
            return Err(Rejection::UnsupportedProtocol);
        }
        if let Some(pointer) = first_null(params, "") {
            return Err(Rejection::Invalid(format!(
                "`{pointer}` is null; leave the field out instead"
            )));
        }
        let file = RequestFile::deserialize(params)
            .map_err(|error| Rejection::Invalid(error.to_string()))?;
        let project = project(file.project)?;
        let deadline = deadline(file.deadline_ms)?;
        check_evidence(&file.evidence, &file.candidates)?;
        let target = match (file.on, file.strategy) {
            (Some(on), None) if on.trim().is_empty() => {
                return Err(Rejection::Invalid("`on` names no event".to_owned()));
            }
            (Some(on), None) if file.judges.is_none() && file.asks.is_none() => Target::On(on),
            (Some(_), None) => {
                return Err(Rejection::Invalid(
                    "`judges` and `asks` come only with an inline `strategy`".to_owned(),
                ));
            }
            (None, Some(steps)) => {
                let strategy = Strategy::build(
                    INLINE,
                    file.judges.unwrap_or_default(),
                    &file.asks.unwrap_or_default(),
                    steps,
                )?;
                if strategy.uses_tier(Tier::SystemTwo) && project.is_none() {
                    return Err(Rejection::JudgeUnavailable(
                        "System Two runs inline only for a `project` listed in projects.json, and this request names no project".to_owned(),
                    ));
                }
                check_named_evidence(&strategy, &file.evidence)?;
                Target::Strategy(strategy)
            }
            _ => {
                return Err(Rejection::Invalid(
                    "a request needs `on` or `strategy`, not both".to_owned(),
                ));
            }
        };
        Ok(Self {
            deadline,
            project,
            evidence: file.evidence.into_iter().collect(),
            candidates: file.candidates.into_iter().collect(),
            target,
        })
    }

    /// How long the rules may take.
    #[must_use]
    pub const fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The project the caller works in, an absolute path; its rules and
    /// System Two allowance apply.
    #[must_use]
    pub fn project(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    /// The named evidence blocks, in name order.
    #[must_use]
    pub fn evidence(&self) -> &[(String, String)] {
        &self.evidence
    }

    /// The named candidates, in name order.
    #[must_use]
    pub fn candidates(&self) -> &[(String, String)] {
        &self.candidates
    }

    /// The event or the inline strategy.
    #[must_use]
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// The event rules are matched against: named by `on`, with each
    /// evidence block as a field. None for an inline strategy.
    #[must_use]
    pub fn event(&self) -> Option<Event> {
        let Target::On(on) = &self.target else {
            return None;
        };
        let fields: Map<String, Value> = self
            .evidence
            .iter()
            .map(|(name, content)| (name.clone(), Value::String(content.clone())))
            .collect();
        Some(Event::new(on.clone(), fields))
    }
}

fn deadline(deadline_ms: Option<u64>) -> Result<Duration, Rejection> {
    let Some(milliseconds) = deadline_ms else {
        return Ok(DEFAULT_DEADLINE);
    };
    if milliseconds == 0 {
        return Err(Rejection::Invalid(
            "`deadline_ms` must be at least 1".to_owned(),
        ));
    }
    Limit::Deadline.check(milliseconds)?;
    Ok(Duration::from_millis(milliseconds))
}

/// The request's `project`, checked to an absolute path within its limit.
fn project(file_value: Option<String>) -> Result<Option<PathBuf>, Rejection> {
    let Some(value) = file_value else {
        return Ok(None);
    };
    Limit::ProjectBytes.check(count(value.len()))?;

    if !Path::new(&value).is_absolute() {
        return Err(Rejection::Invalid(
            "`project` is an absolute path".to_owned(),
        ));
    }

    Ok(Some(PathBuf::from(value)))
}

/// The JSON pointer of the first `null` in `value`, if it holds one, so a
/// request says where its null is instead of leaving serde to describe it.
///
/// Recursion is bounded by `serde_json`'s own nesting limit of 128, which a
/// request cannot exceed on the wire.
fn first_null(value: &Value, pointer: &str) -> Option<String> {
    match value {
        Value::Null => Some(pointer.to_owned()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, item)| first_null(item, &format!("{pointer}/{index}"))),
        Value::Object(fields) => fields.iter().find_map(|(key, field)| {
            let key = key.replace('~', "~0").replace('/', "~1");
            first_null(field, &format!("{pointer}/{key}"))
        }),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// An inline strategy's caller sent the evidence too, so a step naming a
/// block it did not send is a mistake, not a block that may come later.
fn check_named_evidence(
    strategy: &Strategy,
    evidence: &BTreeMap<String, String>,
) -> Result<(), Rejection> {
    let missing = strategy.plan().steps().iter().find_map(|step| {
        step.request()
            .evidence()
            .iter()
            .find(|name| !evidence.contains_key(*name))
            .map(|name| (step.id(), name))
    });
    match missing {
        Some((step, name)) => Err(Rejection::Invalid(format!(
            "step `{step}` names evidence `{name}`, which the request does not send"
        ))),
        None => Ok(()),
    }
}

fn check_evidence(
    evidence: &BTreeMap<String, String>,
    candidates: &BTreeMap<String, String>,
) -> Result<(), Exceeded> {
    Limit::EvidenceBlocks.check(count(evidence.len()))?;
    Limit::Candidates.check(count(candidates.len()))?;
    let bytes = evidence
        .iter()
        .chain(candidates)
        .map(|(name, content)| name.len().saturating_add(content.len()))
        .fold(0_usize, usize::saturating_add);
    Limit::EvidenceBytes.check(count(bytes))
}

/// Why a navigation was not answered with intents or a verdict. Each is a
/// JSON-RPC error whose `data.reason` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// A field is missing, unknown, of the wrong type, or the strategy is not
    /// valid.
    Invalid(String),
    /// A limit was exceeded.
    LimitExceeded(Exceeded),
    /// The request's `protocol` is not one the daemon accepts.
    UnsupportedProtocol,
    /// The daemon has no capacity for the request's source.
    Busy,
    /// An inline strategy did not finish within the deadline.
    Deadline,
    /// An inline strategy names a judge with no model to ask.
    JudgeUnavailable(String),
}

impl Rejection {
    /// The reason's wire name, such as `limit_exceeded`.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::LimitExceeded(_) => "limit_exceeded",
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::Busy => "busy",
            Self::Deadline => "deadline",
            Self::JudgeUnavailable(_) => "judge_unavailable",
        }
    }

    /// The JSON-RPC error object: code, message and `data`.
    #[must_use]
    pub fn to_error(&self) -> Value {
        let code = match self {
            Self::Invalid(_) | Self::LimitExceeded(_) | Self::UnsupportedProtocol => INVALID_PARAMS,
            Self::Busy | Self::Deadline | Self::JudgeUnavailable(_) => SERVER_ERROR,
        };
        let mut data = json!({ "reason": self.reason() });
        if let (Self::LimitExceeded(exceeded), Some(fields)) = (self, data.as_object_mut()) {
            fields.insert("limit".into(), json!(exceeded.limit.name()));
            fields.insert("max".into(), json!(exceeded.limit.max()));
        }
        if let (Self::UnsupportedProtocol, Some(fields)) = (self, data.as_object_mut()) {
            fields.insert("accepted".into(), json!(PROTOCOLS));
        }
        json!({ "code": code, "message": self.to_string(), "data": data })
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) | Self::JudgeUnavailable(reason) => f.write_str(reason),
            Self::LimitExceeded(exceeded) => exceeded.fmt(f),
            Self::UnsupportedProtocol => {
                let accepted: Vec<String> = PROTOCOLS.iter().map(u64::to_string).collect();
                write!(f, "the daemon accepts protocol {}", accepted.join(", "))
            }
            Self::Busy => f.write_str("the daemon is busy: at capacity for this caller"),
            Self::Deadline => f.write_str("the strategy did not finish within its deadline"),
        }
    }
}

impl std::error::Error for Rejection {}

impl From<Exceeded> for Rejection {
    fn from(exceeded: Exceeded) -> Self {
        Self::LimitExceeded(exceeded)
    }
}

impl From<RuleError> for Rejection {
    fn from(error: RuleError) -> Self {
        match error {
            RuleError::Limit { source, .. } => Self::LimitExceeded(source),
            other => Self::Invalid(other.to_string()),
        }
    }
}
