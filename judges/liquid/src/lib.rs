//! Liquid AI's d1.2 decision models over the shared System One HTTP transport.
//!
//! `d1.2-ultra` is the default: on the 2026-11-17 replay of 108 Chauffeur
//! judgments it agreed with the expected action 93% of the time (Jev 85%),
//! made confident mistakes on 2% (Jev 7%), and kept every permission answer
//! safe. Set `LIQUID_DEFAULT_MODEL=d1.2` for the smaller model.

use std::path::PathBuf;
use std::time::Duration;

use chauffeur_core::{Answer, Question, SystemOne, SystemOneError};
pub use chauffeur_judge_http::DEFAULT_TIMEOUT;
use chauffeur_judge_http::{HttpClient, HttpConfig};

pub const DEFAULT_BASE_URL: &str = "https://api.liquid.ai/decisions";
pub const DEFAULT_MODEL: &str = "d1.2-ultra";

#[derive(Clone)]
pub struct LiquidConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub record: Option<PathBuf>,
}

impl LiquidConfig {
    /// `LIQUID_AI_KEY` is required; returns `None` when it is unset.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("LIQUID_AI_KEY")
            .ok()
            .filter(|key| !key.is_empty())?;

        Some(Self {
            base_url: std::env::var("LIQUID_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            api_key,
            model: std::env::var("LIQUID_DEFAULT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into()),
            timeout: DEFAULT_TIMEOUT,
            record: std::env::var_os("CHAUFFEUR_RECORD_JUDGMENTS")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
        })
    }
}

pub struct LiquidClient(HttpClient);

impl LiquidClient {
    /// Must be built outside an async runtime.
    pub fn new(config: LiquidConfig) -> Result<Self, String> {
        HttpClient::new(
            "liquid",
            HttpConfig {
                base_url: config.base_url,
                api_key: config.api_key,
                model: config.model,
                timeout: config.timeout,
                record: config.record,
            },
        )
        .map(Self)
    }
}

impl SystemOne for LiquidClient {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn ask(&mut self, state: &str, questions: &[Question]) -> Result<Vec<Answer>, SystemOneError> {
        self.0.ask(state, questions)
    }

    fn ask_by(
        &mut self,
        state: &str,
        questions: &[Question],
        deadline: std::time::Instant,
    ) -> Result<Vec<Answer>, SystemOneError> {
        self.0.ask_by(state, questions, deadline)
    }
}
