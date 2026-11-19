use std::time::{Duration, Instant};

use chauffeur_core::Failure;
use serde_json::{Map, Value, json};

use super::SystemOneEntry;
use super::wire::{Response, is_noul, read_noul, state};
use crate::confidence::Probabilities;
use crate::format::{Evidence, Format};
use crate::question::Question;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: u64 = 262_144;
const JSON: &str = "application/json; charset=utf-8";

/// What ends a call first.
#[derive(Clone, Copy)]
enum Limit {
    /// The model's own timeout, which the agent holds.
    Model,
    /// The time left before the run's deadline, shorter than the model's
    /// timeout.
    Run(Duration),
}

impl Limit {
    /// The failure of a call that ran past the limit. Only the model's own
    /// timeout lets the tier's next model answer.
    const fn passed(self) -> Failure {
        match self {
            Self::Model => Failure::Timeout,
            Self::Run(_) => Failure::Deadline,
        }
    }
}

/// The `/v1/systemone` API that Jev speaks. It asks yes/no questions with
/// `noul`; any other question fails as invalid.
///
/// One call, from connecting to reading the whole reply, takes no longer
/// than the model's timeout or the time left before the round's deadline,
/// whichever is shorter.
pub struct SystemOne {
    agent: ureq::Agent,
    url: String,
    model: String,
    key: String,
}

impl SystemOne {
    /// A client for `entry`, sending `key` as its bearer token.
    #[must_use]
    pub fn new(entry: &SystemOneEntry, key: String) -> Self {
        let timeout = entry
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}/v1/systemone", entry.endpoint.trim_end_matches('/'));
        Self {
            agent,
            url,
            model: entry.model.clone(),
            key,
        }
    }

    /// What ends a call that must finish by `deadline`. With no deadline the
    /// clock is not read.
    fn limit(&self, deadline: Option<Instant>) -> Result<Limit, Failure> {
        let Some(deadline) = deadline else {
            return Ok(Limit::Model);
        };

        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Failure::Deadline);
        }

        match self.agent.config().timeouts().global {
            Some(timeout) if timeout <= left => Ok(Limit::Model),
            _ => Ok(Limit::Run(left)),
        }
    }

    fn call(&self, body: &Value, deadline: Option<Instant>) -> Result<Response, Failure> {
        let body = serde_json::to_vec(body).map_err(|_| Failure::Unavailable)?;
        let request = self
            .agent
            .post(&self.url)
            .header("authorization", format!("Bearer {}", self.key))
            .content_type(JSON);

        // The time left is read last, after the work that needs no network. A
        // request's own timeout replaces the agent's, so it is set only when
        // it is the shorter of the two.
        let limit = self.limit(deadline)?;
        let request = match limit {
            Limit::Model => request,
            Limit::Run(left) => request.config().timeout_global(Some(left)).build(),
        };
        let mut response = request
            .send(body.as_slice())
            .map_err(|error| failure(&error, limit))?;

        match response.status().as_u16() {
            200..=299 => {}
            429 | 529 => return Err(Failure::RateLimited),
            408 => return Err(Failure::Timeout),
            _ => return Err(Failure::Unavailable),
        }

        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|error| failure(&error, limit))?;

        serde_json::from_str(&text).map_err(|_| Failure::Invalid)
    }
}

/// A transport error as a failure, for a call ended by `limit`. Its text is
/// dropped: it can carry the request's context.
const fn failure(error: &ureq::Error, limit: Limit) -> Failure {
    match error {
        ureq::Error::Timeout(_) => limit.passed(),
        ureq::Error::BodyExceedsLimit(_) => Failure::Invalid,
        _ => Failure::Unavailable,
    }
}

impl Format for SystemOne {
    fn answer_all(
        &self,
        questions: &[&Question],
        evidence: &Evidence,
    ) -> Vec<Result<Probabilities, Failure>> {
        self.answer_all_until(questions, evidence, None)
    }

    fn answer_all_until(
        &self,
        questions: &[&Question],
        evidence: &Evidence,
        deadline: Option<Instant>,
    ) -> Vec<Result<Probabilities, Failure>> {
        let ids: Vec<String> = (0..questions.len())
            .map(|index| format!("q{index}"))
            .collect();
        let mut asked = Map::new();

        for (id, question) in ids.iter().zip(questions) {
            if is_noul(question) {
                asked.insert(
                    id.clone(),
                    json!({ "type": "noul", "instructions": question.instructions() }),
                );
            }
        }

        if asked.is_empty() {
            return questions.iter().map(|_| Err(Failure::Invalid)).collect();
        }

        let body =
            json!({ "model": self.model, "state": state(questions, evidence), "questions": asked });

        match self.call(&body, deadline) {
            Ok(response) => ids
                .iter()
                .zip(questions)
                .map(|(id, question)| {
                    if is_noul(question) {
                        read_noul(question, response.answers.get(id))
                    } else {
                        Err(Failure::Invalid)
                    }
                })
                .collect(),
            Err(failure) => questions.iter().map(|_| Err(failure)).collect(),
        }
    }
}
