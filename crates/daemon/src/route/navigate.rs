use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chauffeur_judges::{Fallback, Tier};
use chauffeur_protocol::{Intent, Navigate, Rejection, Rule, Strategy, Target, progress};
use chauffeur_runtime::{Cancel, Context, EventLog, MAX_EVENTS, Ran, run_strategy};
use serde_json::{Map, Value, json};

use super::{Ask, Router, log_entry};
use crate::state::Loaded;

/// What an inline strategy's run is logged as.
const INLINE: &str = "inline";

impl Router {
    /// Answers a navigation: the intents of every rule its event hooks, or an
    /// inline strategy's verdict. `notify` is given each `chauffeur.event`'s
    /// params as the run goes, at most [`MAX_EVENTS`] of them.
    ///
    /// # Errors
    ///
    /// A [`Rejection`]: `busy` when the daemon's workers cannot take the
    /// strategy, `deadline` or `judge_unavailable` for an inline strategy,
    /// `cancelled` once the caller cancels.
    pub fn navigate(
        &self,
        request: Navigate,
        cancel: &Cancel,
        notify: &(dyn Fn(Value) + Sync),
    ) -> Result<Value, Rejection> {
        let loaded = self
            .loaded()
            .ok_or_else(|| Rejection::JudgeUnavailable("no configuration is loaded".to_owned()))?;
        let deadline = Instant::now()
            .checked_add(request.deadline())
            .ok_or(Rejection::Deadline)?;
        let log = Mutex::new(EventLog::new(MAX_EVENTS));
        let send = |event: Value| {
            let kept = log.lock().is_ok_and(|mut log| log.push(()));
            if kept {
                notify(event);
            }
        };
        let evidence = request.evidence().to_vec();
        let project = request.project().map(Path::to_path_buf);
        let answered = match (request.event(), request.into_target()) {
            (Some(event), Target::On(on)) => {
                let hooked: Vec<Arc<Rule>> = loaded
                    .rules
                    .iter()
                    .filter(|rule| rule.hooks(&event))
                    .cloned()
                    .collect();
                let ask = Ask {
                    event: on,
                    session: String::new(),
                    evidence: Arc::new(evidence),
                    deadline,
                    cancel: cancel.clone(),
                };
                Ok(self.intents(&loaded, &hooked, &ask, &send))
            }
            (_, Target::Strategy(strategy)) => {
                allowed(&loaded, &strategy, project.as_deref())?;
                self.inline(&loaded, strategy, &evidence, deadline, cancel, &send)
            }
            (None, Target::On(_)) => Err(Rejection::Invalid("no event to match".to_owned())),
        };
        if cancel.is_cancelled() {
            return Err(Rejection::Cancelled);
        }
        answered
    }

    /// Every hooked rule's intents; a rule that misses the deadline, fails or
    /// meets no `then` entry gives none, and the rest still return.
    fn intents(
        &self,
        loaded: &Arc<Loaded>,
        hooked: &[Arc<Rule>],
        ask: &Ask,
        send: &(dyn Fn(Value) + Sync),
    ) -> Value {
        for rule in hooked {
            send(progress::rule_matched(rule.id()));
        }
        let ran = |rule: &Rule, done: &super::RuleRun| {
            for event in done
                .run
                .iter()
                .flat_map(|run| progress::run_events(Some(rule.id()), &run.events))
            {
                send(event);
            }
        };
        let intents: Vec<Value> = self
            .run_all(loaded, hooked, ask, &ran)
            .into_iter()
            .filter_map(|(rule, then)| then.map(|then| Intent::new(rule.id(), &then).to_json()))
            .collect();
        json!({ "intents": intents })
    }

    /// An inline strategy's verdict, once every judge it names has a model.
    /// Its project was checked against `projects.json` before it is called.
    fn inline(
        &self,
        loaded: &Arc<Loaded>,
        strategy: Strategy,
        evidence: &[(String, String)],
        deadline: Instant,
        cancel: &Cancel,
        send: &(dyn Fn(Value) + Sync),
    ) -> Result<Value, Rejection> {
        available(&loaded.context, &strategy)?;
        let mut entry = Map::new();
        entry.insert("rule".into(), json!(INLINE));
        entry.insert("event".into(), json!("chauffeur.navigate"));
        entry.insert("session".into(), json!(""));
        let started = Instant::now();
        let ran = run_strategy(
            &loaded.context,
            &Arc::new(strategy),
            evidence,
            deadline,
            cancel,
        );
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        entry.insert("elapsed_ms".into(), json!(elapsed));
        let reason = match &ran {
            Ran::Refused(reason) => Some(reason.clone()),
            Ran::Settled(_) | Ran::Deadline | Ran::Cancelled => None,
        };
        let run = log_entry::ended(&mut entry, ran);
        if let Err(error) = self.audit.append(Value::Object(entry)) {
            eprintln!("chauffeur: a run was not written to the audit log: {error}");
        }
        match (run, reason) {
            (Some(run), _) => {
                for event in progress::run_events(None, &run.events) {
                    send(event);
                }
                Ok(json!({ "verdict": progress::verdict(&run.verdict) }))
            }
            (None, Some(reason)) => Err(refused(reason)),
            // A cancelled run is answered as cancelled by `navigate`.
            (None, None) => Err(Rejection::Deadline),
        }
    }
}

/// Refuses an inline System Two strategy for a project `projects.json` does
/// not list: an inline strategy carries its caller's trust, so only the
/// user's own listed projects may spend System Two.
fn allowed(loaded: &Loaded, strategy: &Strategy, project: Option<&Path>) -> Result<(), Rejection> {
    if !strategy.uses_tier(Tier::SystemTwo) {
        return Ok(());
    }
    // A request naming no project is refused at parse; this keeps the
    // daemon from running one if that check is ever bypassed.
    let Some(project) = project else {
        return Err(Rejection::JudgeUnavailable(
            "System Two runs only for a project listed in projects.json, and this request names no project"
                .to_owned(),
        ));
    };
    if loaded.projects().contains(project) {
        return Ok(());
    }
    Err(Rejection::JudgeUnavailable(format!(
        "System Two runs only for a project listed in projects.json, and {} is not",
        project.display()
    )))
}

/// Refuses a strategy naming a judge with no model to ask, before any runs.
fn available(context: &Context, strategy: &Strategy) -> Result<(), Rejection> {
    for (name, spec) in strategy.judges() {
        let fallback = Fallback::build(
            &context.models,
            spec.tier,
            spec.model.as_deref(),
            &context.keys,
        )
        .map_err(|error| Rejection::JudgeUnavailable(error.to_string()))?;
        if fallback.is_empty() {
            return Err(Rejection::JudgeUnavailable(format!(
                "judge `{name}` has no {} model to ask",
                spec.tier.name()
            )));
        }
    }
    Ok(())
}

/// The rejection for an inline run that never ran: `busy` when the daemon's
/// workers could not take it, `judge_unavailable` when a judge could not run.
/// The audit entry keeps the reason either way.
fn refused(reason: String) -> Rejection {
    if from_pool(&reason) {
        return Rejection::Busy;
    }
    Rejection::JudgeUnavailable(reason)
}

/// Whether `reason` came from the pool rather than a judge:
/// `chauffeur_runtime` refuses a run it cannot start, or whose worker
/// stopped, in these words; a judge's failure is the judge's own message.
fn from_pool(reason: &str) -> bool {
    reason.starts_with("cannot start a worker") || reason == "the run's worker stopped"
}

#[cfg(test)]
mod tests {
    use super::{Rejection, refused};

    /// A refusal from the pool answers `busy`, so a caller is not told a
    /// judge is gone when the daemon was merely busy; a judge's own failure
    /// still answers `judge_unavailable`.
    #[test]
    fn a_pool_refusal_is_busy_and_a_judge_refusal_is_unavailable() {
        assert_eq!(
            refused("cannot start a worker: all 32 workers are busy".to_owned()),
            Rejection::Busy
        );
        assert_eq!(
            refused("the run's worker stopped".to_owned()),
            Rejection::Busy
        );
        assert_eq!(
            refused("no system-one model is registered".to_owned()),
            Rejection::JudgeUnavailable("no system-one model is registered".to_owned())
        );
    }
}
