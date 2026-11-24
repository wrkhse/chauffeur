//! Provider selection and bounded, opt-in judge fallback.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chauffeur_core::{Answer, Question, SystemOne, SystemOneError, validate_answers};
use chauffeur_judge_glide::{GlideClient, GlideConfig};
use chauffeur_judge_jev::{JevClient, JevConfig};
use chauffeur_judge_liquid::{LiquidClient, LiquidConfig};
use chauffeur_judge_subq::{SubqClient, SubqConfig};

use crate::escalation::{Escalate, Review};
use crate::scheduler::SystemTwoFactory;

const RETRY_AFTER: Duration = Duration::from_secs(60);
/// The benchmarked bar: answers under this effective confidence escalate.
const DEFAULT_ESCALATE_BELOW: f32 = 0.6;

/// Builds one System One client for each judge worker.
pub type SystemOneFactory = Arc<dyn Fn() -> Result<Box<dyn SystemOne>, String> + Send + Sync>;

#[derive(Clone)]
enum Provider {
    Jev(JevConfig),
    Subq(SubqConfig),
    Glide(GlideConfig),
    Liquid(LiquidConfig),
    Custom(SystemOneFactory),
}

impl Provider {
    fn build(&self) -> Result<Box<dyn SystemOne>, String> {
        match self {
            Self::Jev(config) => Ok(Box::new(JevClient::new(config.clone())?)),
            Self::Subq(config) => Ok(Box::new(SubqClient::new(config.clone())?)),
            Self::Glide(config) => Ok(Box::new(GlideClient::new(config.clone())?)),
            Self::Liquid(config) => Ok(Box::new(LiquidClient::new(config.clone())?)),
            Self::Custom(factory) => factory(),
        }
    }
}

/// Confidence escalation: a second System One model, the bar below which
/// an answer escalates, and an optional System Two profile after both.
#[derive(Clone)]
struct Escalation {
    to: Provider,
    below: f32,
    review: Option<String>,
}

/// The hosted judges this process can select. Each is present only when its
/// credentials are set; an unused provider sends nothing.
#[derive(Clone, Default)]
struct Providers {
    jev: Option<JevConfig>,
    subq: Option<SubqConfig>,
    glide: Option<GlideConfig>,
    liquid: Option<LiquidConfig>,
}

impl Providers {
    fn from_env() -> Self {
        Self {
            jev: JevConfig::from_env(),
            subq: SubqConfig::from_env(),
            glide: GlideConfig::from_env(),
            liquid: LiquidConfig::from_env(),
        }
    }

    fn provider(&self, name: &str) -> Result<Provider, String> {
        match name {
            "jev" => self
                .jev
                .clone()
                .map(Provider::Jev)
                .ok_or_else(|| "TYPESAFE_API_KEY is required for the selected Jev judge".into()),
            "subq" => self
                .subq
                .clone()
                .map(Provider::Subq)
                .ok_or_else(|| "SUBQ_API_KEY is required for the selected SubQ judge".into()),
            "glide" => {
                self.glide.clone().map(Provider::Glide).ok_or_else(|| {
                    "FASTINO_API_KEY is required for the selected GLiDE judge".into()
                })
            }
            "liquid" => self
                .liquid
                .clone()
                .map(Provider::Liquid)
                .ok_or_else(|| "LIQUID_AI_KEY is required for the selected Liquid judge".into()),
            _ => Err(format!(
                "unknown judge {name:?}; expected jev, subq, glide or liquid"
            )),
        }
    }
}

#[derive(Clone)]
pub struct JudgeConfig {
    primary: Provider,
    fallback: Option<Provider>,
    escalation: Option<Escalation>,
    review: Option<SystemTwoFactory>,
}

impl JudgeConfig {
    /// Jev-only configuration, also used by local daemon fixtures.
    #[must_use]
    pub fn jev(config: JevConfig) -> Self {
        Self {
            primary: Provider::Jev(config),
            fallback: None,
            escalation: None,
            review: None,
        }
    }

    /// A judge built by `factory`, once for each worker, such as a local
    /// model or a test double.
    #[must_use]
    pub fn custom(factory: SystemOneFactory) -> Self {
        Self {
            primary: Provider::Custom(factory),
            fallback: None,
            escalation: None,
            review: None,
        }
    }

    /// Only selected providers require credentials. A key alone never enables
    /// fallback or escalation, because each sends the same redacted context
    /// to another service.
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let providers = Providers::from_env();
        let primary = var("CHAUFFEUR_JUDGE").unwrap_or_else(|| "jev".into());
        let config = Self::select(
            &primary,
            var("CHAUFFEUR_JUDGE_FALLBACK").as_deref(),
            &providers,
        )?;
        let Some(to) = var("CHAUFFEUR_JUDGE_ESCALATE").filter(|name| name != "none") else {
            return Ok(config);
        };

        config.escalating(
            &primary,
            &to,
            var("CHAUFFEUR_JUDGE_ESCALATE_BELOW").as_deref(),
            var("CHAUFFEUR_JUDGE_REVIEW"),
            &providers,
        )
    }

    fn escalating(
        mut self,
        primary: &str,
        to: &str,
        below: Option<&str>,
        review: Option<String>,
        providers: &Providers,
    ) -> Result<Self, String> {
        if to == primary {
            return Err("CHAUFFEUR_JUDGE_ESCALATE must differ from CHAUFFEUR_JUDGE".into());
        }

        let below = match below {
            None => DEFAULT_ESCALATE_BELOW,
            Some(text) => text
                .parse::<f32>()
                .ok()
                .filter(|below| *below > 0.0 && *below <= 1.0)
                .ok_or("CHAUFFEUR_JUDGE_ESCALATE_BELOW must be a number above 0 and at most 1")?,
        };

        self.escalation = Some(Escalation {
            to: providers.provider(to)?,
            below,
            review,
        });
        Ok(self)
    }

    /// The System Two profile that reviews answers both System One judges
    /// doubt, if one is configured, or `None`.
    pub(crate) fn review_profile(&self) -> Option<&str> {
        self.escalation.as_ref()?.review.as_deref()
    }

    /// Give escalation's review step System Two's executors.
    #[must_use]
    pub(crate) fn with_review(mut self, factory: SystemTwoFactory) -> Self {
        self.review = Some(factory);
        self
    }

    fn select(
        primary: &str,
        fallback: Option<&str>,
        providers: &Providers,
    ) -> Result<Self, String> {
        let fallback = fallback.filter(|name| !name.is_empty() && *name != "none");

        if fallback == Some(primary) {
            return Err("CHAUFFEUR_JUDGE_FALLBACK must differ from CHAUFFEUR_JUDGE".into());
        }

        Ok(Self {
            primary: providers.provider(primary)?,
            fallback: fallback.map(|name| providers.provider(name)).transpose()?,
            escalation: None,
            review: None,
        })
    }

    /// Build on a worker thread, never inside an async runtime. Each worker
    /// builds its own, so a fallback's cooldown is per worker.
    pub(crate) fn build(&self) -> Result<Judge, String> {
        let primary = self.primary.build()?;
        let first: Box<dyn SystemOne> = match &self.fallback {
            Some(fallback) => Box::new(Fallback::new(primary, fallback.build()?)),
            None => primary,
        };
        let Some(escalation) = &self.escalation else {
            return Ok(Judge::Plain(first));
        };
        let review = match (&escalation.review, &self.review) {
            (Some(profile), Some(factory)) => Some(Review {
                executor: factory()?,
                profile: profile.clone(),
            }),
            _ => None,
        };

        Ok(Judge::Escalating(Box::new(Escalate::new(
            first,
            escalation.to.build()?,
            escalation.below,
            review,
        ))))
    }
}

/// One worker's judge, as configured.
pub(crate) enum Judge {
    Plain(Box<dyn SystemOne>),
    Escalating(Box<Escalate>),
}

/// One ask's answers, which judges gave them, and the first judge's answers
/// that escalation replaced.
pub(crate) struct Judged {
    pub(crate) result: Result<Vec<Answer>, SystemOneError>,
    pub(crate) judge: String,
    pub(crate) replaced: Vec<Answer>,
}

impl Judge {
    /// An urgent ask, a permission request holding the agent's tool call,
    /// never escalates: its unsure answers abstain, and an escalation there
    /// almost never changed the decision while it added most of a second.
    pub(crate) fn ask(
        &mut self,
        state: &str,
        questions: &[Question],
        deadline: Instant,
        urgent: bool,
    ) -> Judged {
        match self {
            Self::Plain(judge) => Judged {
                result: judge.ask_by(state, questions, deadline),
                judge: judge.name().to_string(),
                replaced: Vec::new(),
            },
            Self::Escalating(judge) => {
                let result = if urgent {
                    judge.ask_first(state, questions, deadline)
                } else {
                    judge.ask_by(state, questions, deadline)
                };

                Judged {
                    result,
                    judge: judge.name().to_string(),
                    replaced: judge.take_replaced(),
                }
            }
        }
    }
}

struct Fallback {
    primary: Box<dyn SystemOne>,
    fallback: Box<dyn SystemOne>,
    retry_at: Option<Instant>,
    last_used: String,
}

impl Fallback {
    fn new(primary: Box<dyn SystemOne>, fallback: Box<dyn SystemOne>) -> Self {
        let last_used = primary.name().to_string();

        Self {
            primary,
            fallback,
            retry_at: None,
            last_used,
        }
    }

    fn ask_validated(
        judge: &mut dyn SystemOne,
        state: &str,
        questions: &[Question],
        deadline: Option<Instant>,
    ) -> Result<Vec<Answer>, SystemOneError> {
        let answers = match deadline {
            Some(deadline) => judge.ask_by(state, questions, deadline)?,
            None => judge.ask(state, questions)?,
        };
        validate_answers(questions, &answers)?;

        Ok(answers)
    }
}

impl SystemOne for Fallback {
    fn name(&self) -> &str {
        &self.last_used
    }

    fn ask(&mut self, state: &str, questions: &[Question]) -> Result<Vec<Answer>, SystemOneError> {
        self.attempt(state, questions, None)
    }

    fn ask_by(
        &mut self,
        state: &str,
        questions: &[Question],
        deadline: Instant,
    ) -> Result<Vec<Answer>, SystemOneError> {
        self.attempt(state, questions, Some(deadline))
    }
}

impl Fallback {
    fn attempt(
        &mut self,
        state: &str,
        questions: &[Question],
        deadline: Option<Instant>,
    ) -> Result<Vec<Answer>, SystemOneError> {
        let mut primary_error = None;

        if self.retry_at.is_none_or(|at| Instant::now() >= at) {
            self.last_used = self.primary.name().to_string();

            match Self::ask_validated(self.primary.as_mut(), state, questions, deadline) {
                Ok(answers) => {
                    self.retry_at = None;
                    return Ok(answers);
                }
                Err(error) => {
                    primary_error = Some(error);
                    self.retry_at = Some(Instant::now() + RETRY_AFTER);
                    eprintln!(
                        "chauffeur: {} judge failed; using {} fallback (primary retry in 60s)",
                        self.primary.name(),
                        self.fallback.name()
                    );
                }
            }
        }

        self.last_used = self.fallback.name().to_string();
        Self::ask_validated(self.fallback.as_mut(), state, questions, deadline).map_err(|error| {
            SystemOneError(format!(
                "{}; {} fallback failed: {}",
                primary_error.map_or_else(
                    || format!("{} judge cooling down", self.primary.name()),
                    |error| error.0
                ),
                self.fallback.name(),
                error.0
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chauffeur_core::{AnswerValue, QuestionKind};
    use std::collections::VecDeque;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fake {
        name: &'static str,
        replies: VecDeque<Result<Vec<Answer>, SystemOneError>>,
        calls: Arc<AtomicUsize>,
    }

    impl SystemOne for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn ask(&mut self, state: &str, _: &[Question]) -> Result<Vec<Answer>, SystemOneError> {
            assert_eq!(state, "redacted [REDACTED]");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies.pop_front().expect("unexpected judge call")
        }
    }

    fn fake(
        name: &'static str,
        replies: Vec<Result<Vec<Answer>, SystemOneError>>,
    ) -> (Box<dyn SystemOne>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Fake {
                name,
                replies: replies.into(),
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn answer(p: f32) -> Result<Vec<Answer>, SystemOneError> {
        Ok(vec![Answer {
            id: "q".into(),
            value: AnswerValue::Noul(p),
            confidence: None,
        }])
    }

    fn ask(judge: &mut Fallback) -> Result<Vec<Answer>, SystemOneError> {
        judge.ask(
            "redacted [REDACTED]",
            &[Question {
                id: "q".into(),
                instructions: "Needed?".into(),
                kind: QuestionKind::Noul,
                facts: chauffeur_core::Facts::new(),
                second_opinion: Default::default(),
            }],
        )
    }

    #[test]
    fn valid_low_confidence_answers_never_trigger_fallback() {
        let (primary, pc) = fake("jev", vec![answer(0.5)]);
        let (fallback, fc) = fake("subq", vec![]);
        let mut judge = Fallback::new(primary, fallback);
        assert_eq!(ask(&mut judge).unwrap()[0].value, AnswerValue::Noul(0.5));
        assert_eq!(pc.load(Ordering::SeqCst), 1);
        assert_eq!(fc.load(Ordering::SeqCst), 0);
        assert_eq!(judge.name(), "jev");
    }

    #[test]
    fn failure_uses_fallback_and_skips_primary_until_recovery_probe() {
        let (primary, pc) = fake(
            "jev",
            vec![Err(SystemOneError("HTTP 503".into())), answer(0.9)],
        );
        let (fallback, fc) = fake("subq", vec![answer(0.8), answer(0.8)]);
        let mut judge = Fallback::new(primary, fallback);
        ask(&mut judge).unwrap();
        assert_eq!(judge.name(), "subq");
        ask(&mut judge).unwrap();
        assert_eq!(pc.load(Ordering::SeqCst), 1);
        assert_eq!(fc.load(Ordering::SeqCst), 2);
        judge.retry_at = Some(Instant::now());
        ask(&mut judge).unwrap();
        assert_eq!(pc.load(Ordering::SeqCst), 2);
        assert_eq!(fc.load(Ordering::SeqCst), 2);
        assert_eq!(judge.name(), "jev");
        assert!(judge.retry_at.is_none());
    }

    #[test]
    fn invalid_primary_answers_fall_back_and_invalid_fallback_fails_closed() {
        let (primary, _) = fake("jev", vec![answer(2.0)]);
        let (fallback, _) = fake("subq", vec![answer(0.9)]);
        assert!(ask(&mut Fallback::new(primary, fallback)).is_ok());
        let (primary, _) = fake("subq", vec![Err(SystemOneError("HTTP 402".into()))]);
        let (fallback, _) = fake("jev", vec![Ok(vec![])]);
        let error = ask(&mut Fallback::new(primary, fallback)).unwrap_err();
        assert!(error.0.contains("HTTP 402"));
        assert!(error.0.contains("jev fallback failed"));
    }

    #[test]
    fn glide_selection_requires_explicit_selection_and_its_key() {
        let glide = GlideConfig {
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            model: "fastino/GLiDE".into(),
            timeout: Duration::from_secs(3),
            record: None,
        };
        let without_key = Providers::default();
        let with_key = Providers {
            glide: Some(glide),
            ..Providers::default()
        };
        assert!(JudgeConfig::select("glide", None, &without_key).is_err());
        assert!(JudgeConfig::select("glide", None, &with_key).is_ok());
        assert!(JudgeConfig::select("glide", Some("glide"), &with_key).is_err());
    }

    #[test]
    fn liquid_selection_requires_its_key() {
        let liquid = LiquidConfig {
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            model: "d1.2-ultra".into(),
            timeout: Duration::from_secs(3),
            record: None,
        };
        let both = Providers {
            jev: Some(JevConfig {
                base_url: "http://localhost".into(),
                api_key: "test".into(),
                model: "jev".into(),
                timeout: Duration::from_secs(3),
                record: None,
            }),
            liquid: Some(liquid),
            ..Providers::default()
        };
        assert!(JudgeConfig::select("liquid", None, &Providers::default()).is_err());
        assert!(JudgeConfig::select("liquid", None, &both).is_ok());
        assert!(JudgeConfig::select("jev", Some("liquid"), &both).is_ok());
    }

    #[test]
    fn escalation_needs_a_distinct_judge_its_key_and_a_bar_in_range() {
        let glide = GlideConfig {
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            model: "fastino/GLiDE".into(),
            timeout: Duration::from_secs(3),
            record: None,
        };
        let base = || JudgeConfig::custom(Arc::new(|| Err("not built".into())));
        let without_key = Providers::default();
        let with_key = Providers {
            glide: Some(glide),
            ..Providers::default()
        };

        let config = base()
            .escalating(
                "jev",
                "glide",
                None,
                Some("system-one-review".into()),
                &with_key,
            )
            .unwrap();
        assert_eq!(
            config.escalation.as_ref().unwrap().below,
            DEFAULT_ESCALATE_BELOW
        );
        assert_eq!(config.review_profile(), Some("system-one-review"));
        assert!(
            base()
                .escalating("jev", "glide", Some("0.8"), None, &with_key)
                .is_ok()
        );
        assert!(
            base()
                .escalating("jev", "glide", None, None, &without_key)
                .is_err()
        );
        assert!(
            base()
                .escalating("glide", "glide", None, None, &with_key)
                .is_err()
        );
        for bar in ["0", "1.5", "high", "NaN"] {
            assert!(
                base()
                    .escalating("jev", "glide", Some(bar), None, &with_key)
                    .is_err()
            );
        }
        assert_eq!(base().review_profile(), None);
    }

    #[test]
    fn selection_requires_only_selected_credentials_and_rejects_typos() {
        let jev = JevConfig {
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            model: "jev".into(),
            timeout: Duration::from_secs(3),
            record: None,
        };
        let subq = SubqConfig {
            base_url: "http://localhost".into(),
            api_key: "test".into(),
            model: "subq".into(),
            timeout: Duration::from_secs(3),
            record: None,
        };
        let both = Providers {
            jev: Some(jev.clone()),
            subq: Some(subq.clone()),
            ..Providers::default()
        };
        let jev_only = Providers {
            jev: Some(jev),
            ..Providers::default()
        };
        let subq_only = Providers {
            subq: Some(subq),
            ..Providers::default()
        };
        assert!(JudgeConfig::select("jev", None, &jev_only).is_ok());
        assert!(JudgeConfig::select("subq", None, &subq_only).is_ok());
        assert!(JudgeConfig::select("jev", Some("subq"), &jev_only).is_err());
        assert!(JudgeConfig::select("subq", Some("jev"), &subq_only).is_err());
        assert!(JudgeConfig::select("jev", Some("subq"), &both).is_ok());
        assert!(JudgeConfig::select("subq", Some("jev"), &both).is_ok());
        assert!(JudgeConfig::select("jev", Some("jev"), &both).is_err());
        assert!(JudgeConfig::select("typo", None, &both).is_err());
        assert!(JudgeConfig::select("jev", Some("typo"), &both).is_err());
        assert!(JudgeConfig::select("jev", Some("none"), &jev_only).is_ok());
    }
}
