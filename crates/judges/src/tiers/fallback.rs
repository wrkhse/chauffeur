use std::time::Instant;

use chauffeur_core::Failure;

use super::{Rests, Tier, system_one};
use crate::confidence::{AnsweredBy, Probabilities};
use crate::config::{ConfigError, ModelEntry, Models};
use crate::format::{Evidence, Format, expired};
use crate::placeholder::Placeholder;
use crate::question::Question;

/// One model, ready to ask.
pub struct Model {
    name: String,
    format: Box<dyn Format>,
}

impl Model {
    /// A model named `name` that speaks `format`.
    pub fn new(name: impl Into<String>, format: Box<dyn Format>) -> Self {
        Self {
            name: name.into(),
            format,
        }
    }
}

/// A tier's models in fallback order. The next model answers only the
/// questions the one before could not: unavailable, rate-limited or timed out.
///
/// A model that fails that way rests for `REST` and is skipped until its
/// rest ends; an answer, even beside such a failure, ends the rest. An invalid
/// reply or a passed deadline never rests a model.
pub struct Fallback {
    models: Vec<Model>,
    rests: Rests,
}

impl Fallback {
    /// Models in the order they are asked, with no model resting.
    #[must_use]
    pub fn new(models: Vec<Model>) -> Self {
        Self {
            models,
            rests: Rests::new(),
        }
    }

    /// The same models, sharing the process's rests. A model that failed in
    /// another tier is skipped here too until its rest ends.
    #[must_use]
    pub fn with_rests(mut self, rests: Rests) -> Self {
        self.rests = rests;
        self
    }

    /// The tier's models from `models`, with `preferred` asked first when it
    /// is registered. A model whose key `key` cannot find is left out, so a
    /// shipped model is used once its key is present.
    ///
    /// # Errors
    ///
    /// A placeholder's answers cannot be loaded.
    pub fn build(
        models: &Models,
        tier: Tier,
        preferred: Option<&str>,
        key: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let order = preferred
            .into_iter()
            .chain(models.tier(tier).iter().map(String::as_str));
        let mut built: Vec<Model> = Vec::new();
        for name in order {
            if built.iter().any(|model| model.name == name) {
                continue;
            }
            let format: Box<dyn Format> = match models.model(name) {
                Some(ModelEntry::Systemone(entry)) => {
                    match key(&entry.key.env).and_then(|key| system_one::format(entry, key)) {
                        Some(format) => format,
                        None => continue,
                    }
                }
                Some(ModelEntry::Placeholder(entry)) => Box::new(Placeholder::load(entry)?),
                None => continue,
            };
            built.push(Model::new(name, format));
        }
        Ok(Self::new(built))
    }

    /// Whether no model is left to ask.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// One result per question, in order. With no model left to ask, a
    /// question fails as unavailable.
    #[must_use]
    pub fn answer_all(
        &self,
        questions: &[&Question],
        evidence: &Evidence,
    ) -> Vec<Result<(Probabilities, AnsweredBy), Failure>> {
        self.answer_all_until(questions, evidence, None)
    }

    /// One result per question, in order, with every model's work ending at
    /// `deadline`: the models share the one moment, so a slow model leaves
    /// the next only what is left. Once it passes, the questions still open
    /// fail as [`Failure::Deadline`] and no further model is asked; answers
    /// and failures already final stand. `None` is no deadline: without a
    /// failing model, nothing reads the clock.
    ///
    /// A model that fails as unavailable, rate-limited or timed out rests for
    /// `REST` and is skipped while it rests. An answer ends its rest, even
    /// beside such a failure in the same round; an invalid reply or a passed
    /// deadline does not rest it. With no deadline, no clock is read until a
    /// model fails; after a rest starts, later rounds check it.
    #[must_use]
    pub fn answer_all_until(
        &self,
        questions: &[&Question],
        evidence: &Evidence,
        deadline: Option<Instant>,
    ) -> Vec<Result<(Probabilities, AnsweredBy), Failure>> {
        let mut results: Vec<Result<(Probabilities, AnsweredBy), Failure>> = questions
            .iter()
            .map(|_| Err(Failure::Unavailable))
            .collect();
        let mut open: Vec<usize> = (0..questions.len()).collect();

        for model in &self.models {
            if open.is_empty() {
                break;
            }

            if expired(deadline) {
                for index in &open {
                    if let Some(slot) = results.get_mut(*index) {
                        *slot = Err(Failure::Deadline);
                    }
                }

                break;
            }

            if self.rests.cooling(&model.name) {
                continue;
            }

            let asked: Vec<&Question> = open
                .iter()
                .filter_map(|index| questions.get(*index).copied())
                .collect();
            let replies = model.format.answer_all_until(&asked, evidence, deadline);

            if replies
                .iter()
                .any(|reply| matches!(reply, Err(f) if f.falls_back()))
            {
                self.rests.rest(&model.name);
            }
            if replies.iter().any(Result::is_ok) {
                self.rests.clear(&model.name);
            }

            let by = AnsweredBy {
                model: model.name.clone(),
                placeholder: model.format.is_placeholder(),
            };

            for (index, reply) in open.iter().zip(replies) {
                if let Some(slot) = results.get_mut(*index) {
                    *slot = reply.map(|p| (p, by.clone()));
                }
            }

            open.retain(
                |index| matches!(results.get(*index), Some(Err(failure)) if failure.falls_back()),
            );
        }

        results
    }
}
