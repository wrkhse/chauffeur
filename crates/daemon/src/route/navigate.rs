use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chauffeur_judges::{Fallback, Tier};
use chauffeur_protocol::{Intent, Navigate, Rejection, Rule, Strategy, Target, progress};
use chauffeur_runtime::{Cancel, Context, EventLog, MAX_EVENTS, Ran, run_strategy};
use serde_json::{Map, Value, json};

use super::{Ask, Hook, Router, log_entry};
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
                let project_rules = project
                    .as_deref()
                    .map(|path| self.projects.rules(&loaded, path));
                let hooked: Vec<Hook> = loaded
                    .rules
                    .iter()
                    .cloned()
                    .map(|rule| Hook {
                        rule,
                        project: None,
                    })
                    .chain(
                        project_rules
                            .iter()
                            .flat_map(|rules| rules.rules.iter())
                            .cloned()
                            .map(|rule| Hook {
                                rule,
                                project: project.clone(),
                            }),
                    )
                    .filter(|hook| hook.rule.hooks(&event))
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
    /// meets no `then` entry gives none, and the rest still return. A rule is
    /// announced as its run starts, not when it is matched; a run cut short
    /// after that settles each of its steps with its failure word.
    fn intents(
        &self,
        loaded: &Arc<Loaded>,
        hooked: &[Hook],
        ask: &Ask,
        send: &(dyn Fn(Value) + Sync),
    ) -> Value {
        let announced = |hook: &Hook| send(progress::rule_matched(hook.rule.id()));
        let ran = |rule: &Rule, done: &super::RuleRun| {
            for event in done
                .run
                .iter()
                .flat_map(|run| progress::run_events(Some(rule.id()), &run.events))
            {
                send(event);
            }
            if let Some(failed) = done.unsettled {
                for step in rule.plan().steps() {
                    send(progress::settled_failed(rule.id(), step.id(), failed));
                }
            }
        };
        let intents: Vec<Value> = self
            .run_all(loaded, hooked, ask, &announced, &ran)
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
        let refusal = refused(&ran);
        let run = log_entry::ended(&mut entry, ran);
        if let Err(error) = self.audit.append(Value::Object(entry)) {
            eprintln!("chauffeur: a run was not written to the audit log: {error}");
        }
        match (run, refusal) {
            (Some(run), _) => {
                for event in progress::run_events(None, &run.events) {
                    send(event);
                }
                Ok(json!({ "verdict": progress::verdict(&run.verdict) }))
            }
            (None, Some(rejection)) => Err(rejection),
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

/// The rejection for an inline run that did not settle: `busy` when the
/// pool could not start it, `judge_unavailable` when its worker stopped, a
/// judge could not run, or the run ended without a result. The audit entry
/// keeps the reason either way.
fn refused(ran: &Ran) -> Option<Rejection> {
    match ran {
        Ran::Busy => Some(Rejection::Busy),
        Ran::Stopped => Some(Rejection::JudgeUnavailable(
            "the run's worker stopped".to_owned(),
        )),
        Ran::Refused(reason) => Some(Rejection::JudgeUnavailable(reason.clone())),
        Ran::Settled(_) | Ran::Deadline | Ran::Cancelled => None,
        // A variant a newer runtime adds: its run had no result, so it is
        // unavailable rather than busy.
        _ => Some(Rejection::JudgeUnavailable(
            "the run ended without a result".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use chauffeur_runtime::Ran;

    use super::{Rejection, refused};

    /// A run no worker could start answers `busy`: the daemon was at
    /// capacity, not missing a judge.
    #[test]
    fn a_busy_pool_is_busy() {
        assert_eq!(refused(&Ran::Busy), Some(Rejection::Busy));
    }

    /// A worker that stopped is a failure, not capacity, so a caller is not
    /// told the daemon was busy.
    #[test]
    fn a_stopped_worker_is_unavailable() {
        assert_eq!(
            refused(&Ran::Stopped),
            Some(Rejection::JudgeUnavailable(
                "the run's worker stopped".to_owned()
            ))
        );
    }

    /// A judge's own failure keeps its message, as it did before.
    #[test]
    fn a_judge_refusal_keeps_its_reason() {
        assert_eq!(
            refused(&Ran::Refused(
                "no system-one model is registered".to_owned()
            )),
            Some(Rejection::JudgeUnavailable(
                "no system-one model is registered".to_owned()
            ))
        );
    }
}
