//! A request to judge, parsed once at the boundary and bounded.
//!
//! The limits follow the pattern of origin/v2 crates/protocol/src/limits.rs
//! at f5cd49c: each bound is a named constant, and a breach names the limit
//! and its maximum.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The most bytes of raw request text, checked before parsing.
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
/// The most evidence blocks one request carries.
pub const MAX_EVIDENCE_BLOCKS: usize = 64;
/// The most bytes of evidence names and values together.
pub const MAX_EVIDENCE_BYTES: usize = 262_144;
/// The most bytes of the id, session, hook, or one evidence name or value.
pub const MAX_TEXT_BYTES: usize = 16_384;
/// The longest deadline a request may ask for, in milliseconds.
pub const MAX_DEADLINE_MS: u64 = 30_000;
/// The deadline of a request that names none, in milliseconds.
pub const DEFAULT_DEADLINE_MS: u64 = 3_000;

/// Why a request is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The request is malformed.
    Invalid(String),
    /// A part of the request exceeds its limit.
    Limit {
        /// The part that exceeds it.
        name: &'static str,
        /// The limit.
        max: u64,
    },
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid request: {message}"),
            Self::Limit { name, max } => write!(f, "{name} exceeds {max}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// The request as it arrives on the wire.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    #[serde(default)]
    id: Option<String>,
    session: String,
    hook: String,
    subject: Map<String, Value>,
    #[serde(default)]
    evidence: BTreeMap<String, String>,
    #[serde(default)]
    deadline_ms: Option<u64>,
}

/// A validated request: every field is within its limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    id: String,
    session: String,
    hook: String,
    subject: Map<String, Value>,
    evidence: BTreeMap<String, String>,
    deadline_ms: u64,
}

impl Request {
    /// Parses and validates a request from JSON.
    ///
    /// A request without an id gets one derived from its content, so the
    /// same request always has the same id.
    ///
    /// # Errors
    ///
    /// Returns [`RequestError::Invalid`] for malformed JSON, unknown fields,
    /// empty text or a zero deadline, and [`RequestError::Limit`] for
    /// text over [`MAX_REQUEST_BYTES`] or a part over its limit.
    pub fn parse(json: &str) -> Result<Self, RequestError> {
        if json.len() > MAX_REQUEST_BYTES {
            return Err(limit("request bytes", MAX_REQUEST_BYTES));
        }

        let parsed = serde_json::from_str::<RequestFile>(json);
        let mut file = parsed.map_err(|error| invalid(&error))?;

        check_text("session", &file.session)?;
        check_text("hook", &file.hook)?;

        if let Some(id) = &file.id {
            check_text("id", id)?;
        }

        check_evidence(&file.evidence)?;
        let deadline_ms = check_deadline(file.deadline_ms)?;

        let id = file
            .id
            .take()
            .unwrap_or_else(|| derive_id(&file, deadline_ms));

        Ok(Self {
            id,
            session: file.session,
            hook: file.hook,
            subject: file.subject,
            evidence: file.evidence,
            deadline_ms,
        })
    }

    /// The request's id, given or derived.
    #[must_use]
    pub const fn id(&self) -> &str {
        self.id.as_str()
    }

    /// The session the request belongs to.
    #[must_use]
    pub const fn session(&self) -> &str {
        self.session.as_str()
    }

    /// The hook that raised the request, such as `tool_after`.
    #[must_use]
    pub const fn hook(&self) -> &str {
        self.hook.as_str()
    }

    /// The subject value at a dot-separated path, such as `input.command`.
    ///
    /// Each segment after the first steps into an object.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        let mut segments = name.split('.');
        let first = self.subject.get(segments.next()?)?;

        segments.try_fold(first, |value, segment| value.as_object()?.get(segment))
    }

    /// The named evidence blocks.
    #[must_use]
    pub const fn evidence(&self) -> &BTreeMap<String, String> {
        &self.evidence
    }

    /// How long the caller waits for a verdict, in milliseconds.
    #[must_use]
    pub const fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
}

fn invalid(error: &serde_json::Error) -> RequestError {
    RequestError::Invalid(error.to_string())
}

fn limit(name: &'static str, max: usize) -> RequestError {
    RequestError::Limit {
        name,
        max: u64::try_from(max).unwrap_or(u64::MAX),
    }
}

fn check_text(name: &'static str, value: &str) -> Result<(), RequestError> {
    if value.is_empty() {
        return Err(RequestError::Invalid(format!("{name} is empty")));
    }

    if value.len() > MAX_TEXT_BYTES {
        return Err(limit(name, MAX_TEXT_BYTES));
    }

    Ok(())
}

fn check_evidence(evidence: &BTreeMap<String, String>) -> Result<(), RequestError> {
    if evidence.len() > MAX_EVIDENCE_BLOCKS {
        return Err(limit("evidence blocks", MAX_EVIDENCE_BLOCKS));
    }

    let mut total: usize = 0;

    for (name, value) in evidence {
        if name.len() > MAX_TEXT_BYTES {
            return Err(limit("evidence name", MAX_TEXT_BYTES));
        }

        if value.len() > MAX_TEXT_BYTES {
            return Err(limit("evidence value", MAX_TEXT_BYTES));
        }

        total = total.saturating_add(name.len()).saturating_add(value.len());
    }

    if total > MAX_EVIDENCE_BYTES {
        return Err(limit("evidence bytes", MAX_EVIDENCE_BYTES));
    }

    Ok(())
}

fn check_deadline(deadline_ms: Option<u64>) -> Result<u64, RequestError> {
    match deadline_ms {
        None => Ok(DEFAULT_DEADLINE_MS),
        Some(0) => Err(RequestError::Invalid("deadline_ms is zero".to_owned())),
        Some(ms) if ms > MAX_DEADLINE_MS => Err(RequestError::Limit {
            name: "deadline_ms",
            max: MAX_DEADLINE_MS,
        }),
        Some(ms) => Ok(ms),
    }
}

/// Derives `r` and 16 lowercase hex digits from the request's content.
///
/// The content is the canonical JSON of its fields; `serde_json` keeps object
/// keys sorted, so equal requests give equal text.
fn derive_id(file: &RequestFile, deadline_ms: u64) -> String {
    let mut content = Map::new();
    content.insert("session".to_owned(), Value::from(file.session.clone()));
    content.insert("hook".to_owned(), Value::from(file.hook.clone()));
    content.insert("subject".to_owned(), Value::Object(file.subject.clone()));
    content.insert("evidence".to_owned(), evidence_value(&file.evidence));
    content.insert("deadline_ms".to_owned(), Value::from(deadline_ms));

    format!(
        "r{:016x}",
        fnv1a(Value::Object(content).to_string().as_bytes())
    )
}

fn evidence_value(evidence: &BTreeMap<String, String>) -> Value {
    evidence
        .iter()
        .map(|(name, text)| (name.clone(), Value::from(text.clone())))
        .collect::<Map<String, Value>>()
        .into()
}

/// FNV-1a, 64-bit, with the published offset basis and prime.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(value: &Value) -> Result<Request, RequestError> {
        Request::parse(&value.to_string())
    }

    fn minimal() -> Value {
        json!({ "session": "s1", "hook": "tool_after", "subject": {} })
    }

    fn with(key: &str, extra: Value) -> Value {
        let mut value = minimal();
        value.as_object_mut().unwrap().insert(key.to_owned(), extra);
        value
    }

    fn limit(name: &'static str, max: usize) -> RequestError {
        RequestError::Limit {
            name,
            max: u64::try_from(max).unwrap(),
        }
    }

    /// The minimal request with these evidence blocks.
    fn with_evidence(blocks: impl IntoIterator<Item = (String, String)>) -> Value {
        let blocks = blocks.into_iter().map(|(name, text)| (name, json!(text)));

        with("evidence", Value::Object(blocks.collect()))
    }

    /// Evidence of exactly `MAX_EVIDENCE_BYTES` plus `extra` bytes: 16
    /// blocks of a 3-byte name and a value filling the block to
    /// `MAX_TEXT_BYTES`, with `extra` bytes added to the first value.
    fn full_evidence(extra: usize) -> Value {
        let blocks = MAX_EVIDENCE_BYTES / MAX_TEXT_BYTES;
        let fill = MAX_TEXT_BYTES - 3;

        with_evidence((0..blocks).map(|index| {
            let len = if index == 0 {
                fill.checked_add(extra).unwrap()
            } else {
                fill
            };

            (format!("b{index:02}"), "x".repeat(len))
        }))
    }

    /// The minimal request padded with trailing spaces to `len` bytes.
    fn padded(len: usize) -> String {
        let text = minimal().to_string();
        let padding = " ".repeat(len.checked_sub(text.len()).unwrap());

        text + &padding
    }

    #[test]
    fn over_long_request_hits_the_request_limit() {
        let error = Request::parse(&padded(MAX_REQUEST_BYTES + 1)).unwrap_err();

        assert_eq!(error, limit("request bytes", MAX_REQUEST_BYTES));
        assert_eq!(error.to_string(), "request bytes exceeds 524288");
    }

    #[test]
    fn request_at_the_bound_still_parses() {
        let request = Request::parse(&padded(MAX_REQUEST_BYTES)).unwrap();

        assert_eq!(request, parse(&minimal()).unwrap());
    }

    #[test]
    fn minimal_request_round_trips_with_a_derived_id() {
        let request = parse(&minimal()).unwrap();
        let again = parse(&minimal()).unwrap();

        assert_eq!(request, again);
        // FNV-1a of {"deadline_ms":3000,"evidence":{},"hook":"tool_after",
        // "session":"s1","subject":{}}, computed outside this crate.
        assert_eq!(request.id(), "r73031f2691553a88");
        assert_eq!(request.session(), "s1");
        assert_eq!(request.hook(), "tool_after");
        assert_eq!(request.evidence(), &BTreeMap::new());
        assert_eq!(request.deadline_ms(), DEFAULT_DEADLINE_MS);
    }

    #[test]
    fn derived_id_follows_the_content() {
        let one = parse(&minimal()).unwrap();
        let other = parse(&with("subject", json!({ "tool": "shell" }))).unwrap();

        assert_ne!(one.id(), other.id());
    }

    #[test]
    fn explicit_id_is_kept() {
        let request = parse(&with("id", json!("call-7"))).unwrap();

        assert_eq!(request.id(), "call-7");
    }

    #[test]
    fn unknown_field_is_invalid() {
        let raw = r#"{"session":"s1","hook":"h","subject":{},"extra":1}"#;
        let error = Request::parse(raw).unwrap_err();

        assert_eq!(
            error,
            RequestError::Invalid(
                "unknown field `extra`, expected one of `id`, `session`, \
                 `hook`, `subject`, `evidence`, `deadline_ms` at line 1 \
                 column 47"
                    .to_owned(),
            ),
        );
    }

    #[test]
    fn empty_session_is_invalid() {
        let error = parse(&with("session", json!(""))).unwrap_err();

        assert_eq!(error, RequestError::Invalid("session is empty".to_owned()));
        assert_eq!(error.to_string(), "invalid request: session is empty");
    }

    #[test]
    fn oversized_evidence_value_hits_the_text_limit() {
        let evidence = json!({ "diff": "x".repeat(MAX_TEXT_BYTES + 1) });
        let error = parse(&with("evidence", evidence)).unwrap_err();

        assert_eq!(error, limit("evidence value", MAX_TEXT_BYTES));
        assert_eq!(error.to_string(), "evidence value exceeds 16384");
    }

    #[test]
    fn oversized_evidence_hits_the_combined_limit() {
        let blocks: Map<String, Value> = (0..20)
            .map(|index| (format!("b{index:02}"), json!("x".repeat(16_000))))
            .collect();
        let error = parse(&with("evidence", Value::Object(blocks))).unwrap_err();

        assert_eq!(error, limit("evidence bytes", MAX_EVIDENCE_BYTES));
    }

    #[test]
    fn too_many_evidence_blocks_hit_the_block_limit() {
        let blocks: Map<String, Value> = (0..=MAX_EVIDENCE_BLOCKS)
            .map(|index| (format!("b{index}"), json!("x")))
            .collect();
        let error = parse(&with("evidence", Value::Object(blocks))).unwrap_err();

        assert_eq!(error, limit("evidence blocks", MAX_EVIDENCE_BLOCKS));
    }

    #[test]
    fn session_at_the_text_bound_parses_and_one_byte_more_does_not() {
        let session = "s".repeat(MAX_TEXT_BYTES);
        let request = parse(&with("session", json!(session))).unwrap();
        let longer = json!("s".repeat(MAX_TEXT_BYTES + 1));
        let error = parse(&with("session", longer)).unwrap_err();

        assert_eq!(request.session(), session);
        assert_eq!(error, limit("session", MAX_TEXT_BYTES));
    }

    #[test]
    fn evidence_blocks_at_the_bound_parse() {
        let blocks = (0..MAX_EVIDENCE_BLOCKS)
            .map(|index| (format!("b{index:02}"), "x".to_owned()));
        let request = parse(&with_evidence(blocks)).unwrap();

        assert_eq!(request.evidence().len(), MAX_EVIDENCE_BLOCKS);
    }

    #[test]
    fn evidence_name_at_the_text_bound_parses_and_one_byte_more_does_not() {
        let name = "n".repeat(MAX_TEXT_BYTES);
        let block = (name.clone(), "x".to_owned());
        let request = parse(&with_evidence([block])).unwrap();
        let longer = ("n".repeat(MAX_TEXT_BYTES + 1), "x".to_owned());
        let error = parse(&with_evidence([longer])).unwrap_err();

        assert_eq!(
            request.evidence(),
            &BTreeMap::from([(name, "x".to_owned())])
        );
        assert_eq!(error, limit("evidence name", MAX_TEXT_BYTES));
    }

    #[test]
    fn evidence_value_at_the_text_bound_parses_and_one_byte_more_does_not() {
        let value = "v".repeat(MAX_TEXT_BYTES);
        let block = ("diff".to_owned(), value.clone());
        let request = parse(&with_evidence([block])).unwrap();
        let longer = ("diff".to_owned(), "v".repeat(MAX_TEXT_BYTES + 1));
        let error = parse(&with_evidence([longer])).unwrap_err();

        assert_eq!(
            request.evidence(),
            &BTreeMap::from([("diff".to_owned(), value)])
        );
        assert_eq!(error, limit("evidence value", MAX_TEXT_BYTES));
    }

    #[test]
    fn evidence_at_the_byte_bound_parses_and_one_byte_more_does_not() {
        let request = parse(&full_evidence(0)).unwrap();
        let total: usize = request
            .evidence()
            .iter()
            .map(|(name, text)| name.len() + text.len())
            .sum();
        let error = parse(&full_evidence(1)).unwrap_err();

        assert_eq!(total, MAX_EVIDENCE_BYTES);
        assert_eq!(error, limit("evidence bytes", MAX_EVIDENCE_BYTES));
    }

    #[test]
    fn deadline_at_the_max_parses_and_keeps_it() {
        let request = parse(&with("deadline_ms", json!(MAX_DEADLINE_MS))).unwrap();

        assert_eq!(request.deadline_ms(), MAX_DEADLINE_MS);
    }

    #[test]
    fn zero_deadline_is_invalid() {
        let error = parse(&with("deadline_ms", json!(0))).unwrap_err();

        assert_eq!(
            error,
            RequestError::Invalid("deadline_ms is zero".to_owned())
        );
    }

    #[test]
    fn deadline_over_the_max_hits_the_limit() {
        let late = json!(MAX_DEADLINE_MS + 1);
        let error = parse(&with("deadline_ms", late)).unwrap_err();

        assert_eq!(
            error,
            RequestError::Limit {
                name: "deadline_ms",
                max: MAX_DEADLINE_MS
            },
        );
        assert_eq!(error.to_string(), "deadline_ms exceeds 30000");
    }

    #[test]
    fn field_finds_a_nested_subject_value() {
        let subject = json!({ "tool": "shell", "input": { "command": "ls" } });
        let request = parse(&with("subject", subject)).unwrap();

        assert_eq!(request.field("tool"), Some(&json!("shell")));
        assert_eq!(request.field("input.command"), Some(&json!("ls")));
        assert_eq!(request.field("input.missing"), None);
        assert_eq!(request.field("tool.command"), None);
    }

    #[test]
    fn accessors_return_the_parsed_values() {
        let request = Request::parse(
            r#"{"id":"c1","session":"s9","hook":"tool_before",
                "subject":{"tool":"read"},"evidence":{"diff":"+a"},
                "deadline_ms":500}"#,
        )
        .unwrap();
        let evidence = BTreeMap::from([("diff".to_owned(), "+a".to_owned())]);

        assert_eq!(request.id(), "c1");
        assert_eq!(request.session(), "s9");
        assert_eq!(request.hook(), "tool_before");
        assert_eq!(request.evidence(), &evidence);
        assert_eq!(request.deadline_ms(), 500);
    }
}
