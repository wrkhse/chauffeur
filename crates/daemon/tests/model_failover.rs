//! End to end through the engine thread: a usage-limit signal reaches the
//! model router, Jev (over real HTTP) picks a same-tier model, and a switch
//! effect comes back.

mod common;

use chauffeur_core::{AvailableModel, Effect, ModelRef, Signal, SignalKind};
use chauffeur_daemon::{EngineHandle, EngineOptions};
use serde_json::{Value, json};

/// What Jev answers: a choice at a confidence.
struct Judged {
    choice: &'static str,
    confidence: f64,
}

/// Jev's answer: `expected` must be offered, `other_tier` must not.
fn answer(request: &Value, expected: &str, other_tier: &str, judged: &Judged) -> Value {
    let criteria = request["questions"]["model-router/choice"]["criteria"]
        .as_object()
        .expect("criteria");

    assert!(criteria.contains_key(expected), "{criteria:?}");
    assert!(!criteria.contains_key(other_tier), "{criteria:?}");
    assert!(criteria.contains_key("stay"));
    assert!(
        request["state"]
            .as_str()
            .expect("state")
            .contains("Ran tool edit.")
    );

    json!({"model-router/choice": {"type": "choice", "confidence": judged.confidence, "choice": judged.choice}})
}

fn model(provider: &str, id: &str) -> ModelRef {
    ModelRef {
        provider: provider.into(),
        model: id.into(),
        variant: None,
    }
}

fn at(provider: &str, id: &str, variant: &str) -> ModelRef {
    ModelRef {
        variant: Some(variant.into()),
        ..model(provider, id)
    }
}

fn signal(at: u64, kind: SignalKind) -> Signal {
    Signal {
        agent_id: "ses_test".into(),
        at,
        kind,
    }
}

/// A usage limit on `from` switches the agent to `to`, Jev having been
/// offered `to` and not `other_tier`, whatever Jev judged.
async fn fails_over(from: ModelRef, to: ModelRef, other_tier: &'static str, judged: Judged) {
    let expected = to.key();
    let (jev, server) =
        common::serve_jev(move |request| answer(request, &expected, other_tier, &judged));
    let config_dir = std::env::temp_dir().join("chauffeur-daemon-test-config-absent");
    let engine = EngineHandle::spawn(EngineOptions {
        // No skills folder: no permission contracts or rules.
        skills_dir: std::env::temp_dir().join("chauffeur-no-skills-repo"),
        config_dir,
        judge: chauffeur_daemon::JudgeConfig::jev(jev),
        system_two: None,
        idle_reminders: false,
        sourcefed: None,
        disabled: Vec::new(),
        state_file: None,
        audit_file: None,
    })
    .await
    .expect("engine starts");

    let tool = signal(
        1,
        SignalKind::ToolResult {
            tool: "edit".into(),
            ok: true,
            workspace: String::new(),
            subagent: false,
            input: String::new(),
            error: String::new(),
            user_request: String::new(),
            evidence: String::new(),
            candidates: Vec::new(),
        },
    );

    assert!(engine.ingest(tool).await.expect("tool signal").is_empty());

    // In OpenCode's alphabetical order.
    let available = [
        model("anthropic", "claude-fable-5-2"),
        model("anthropic", "claude-opus-5-5"),
        model("anthropic", "claude-sonnet-5-5"),
        model("openai", "gpt-6-astra"),
        model("openai", "gpt-6-luna"),
        model("openai", "gpt-6.2-sol"),
        model("openai", "gpt-6.1-terra"),
    ]
    .into_iter()
    .map(|model| AvailableModel {
        model,
        usable: true,
    })
    .collect();
    let limit = signal(
        2,
        SignalKind::ModelError {
            model: from,
            error_type: "rate_limit_error".into(),
            status: Some(429),
            message: "rate limit exceeded, retry in 20s".into(),
            tool_executed: false,
            available,
        },
    );
    let effects = engine.ingest(limit).await.expect("limit signal");

    assert_eq!(
        effects,
        vec![Effect::Model {
            agent_id: "ses_test".into(),
            model: Some(to)
        }]
    );

    server.join().expect("fixture");
}

/// A switch over the budget compacts through the engine thread from facts
/// alone: no judge is asked, and the summary model is the cheap one.
#[tokio::test]
async fn a_long_context_is_summarized_before_a_model_switch() {
    use chauffeur_core::ContextResumeReason;
    use chauffeur_judge_jev::JevConfig;
    use std::time::Duration;

    let engine = EngineHandle::spawn(EngineOptions {
        skills_dir: std::env::temp_dir().join("chauffeur-no-skills-repo"),
        config_dir: std::env::temp_dir().join("chauffeur-daemon-test-config-absent"),
        judge: chauffeur_daemon::JudgeConfig::jev(JevConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "unused".into(),
            model: "unused".into(),
            timeout: Duration::from_millis(1),
            record: None,
        }),
        system_two: None,
        idle_reminders: false,
        sourcefed: None,
        disabled: Vec::new(),
        state_file: None,
        audit_file: None,
    })
    .await
    .expect("engine starts");

    let available = ["anthropic/claude-opus-5-5", "anthropic/claude-haiku-5"]
        .into_iter()
        .map(|key| {
            let (provider, id) = key.split_once('/').expect("provider/model");

            AvailableModel {
                model: model(provider, id),
                usable: true,
            }
        })
        .collect();

    let switched = signal(
        2,
        SignalKind::ContextResumed {
            model: model("anthropic", "claude-opus-5-5"),
            context_tokens: 100_000,
            context_window: 1_000_000,
            idle_seconds: 0,
            cache_seconds: None,
            reason: ContextResumeReason::ModelSwitch,
            available,
        },
    );

    let effects = engine.ingest(switched).await.expect("switch signal");

    assert_eq!(
        effects,
        vec![Effect::Compact {
            agent_id: "ses_test".into(),
            model: model("anthropic", "claude-haiku-5"),
        }]
    );
}
/// Jev picks the same-tier model.
#[tokio::test]
async fn a_frontier_supervisor_fails_over_to_frontier_on_the_other_provider() {
    fails_over(
        at("anthropic", "claude-fable-5-2", "high"),
        at("openai", "gpt-6-astra", "high"),
        "openai/gpt-6.2-sol#high",
        Judged {
            choice: "openai/gpt-6-astra#high",
            confidence: 0.8,
        },
    )
    .await;
}

/// Jev picks the same-tier model.
#[tokio::test]
async fn a_balanced_worker_fails_over_to_balanced_on_the_other_provider() {
    fails_over(
        at("anthropic", "claude-opus-5-5", "high"),
        at("openai", "gpt-6.2-sol", "high"),
        "openai/gpt-6-astra#high",
        Judged {
            choice: "openai/gpt-6.2-sol#high",
            confidence: 0.8,
        },
    )
    .await;
}

/// Jev's pick is below the bar: the recommended same-tier model wins.
#[tokio::test]
async fn an_unsure_judge_still_sends_astra_to_fable() {
    fails_over(
        at("openai", "gpt-6-astra", "high"),
        at("anthropic", "claude-fable-5-2", "high"),
        "anthropic/claude-opus-5-5#high",
        Judged {
            choice: "anthropic/claude-sonnet-5-5#high",
            confidence: 0.1,
        },
    )
    .await;
}

/// Jev's pick is below the bar: the recommended same-tier model wins.
#[tokio::test]
async fn an_unsure_judge_still_sends_sol_to_opus() {
    fails_over(
        at("openai", "gpt-6.2-sol", "high"),
        at("anthropic", "claude-opus-5-5", "high"),
        "anthropic/claude-fable-5-2#high",
        Judged {
            choice: "anthropic/claude-sonnet-5-5#high",
            confidence: 0.1,
        },
    )
    .await;
}

/// Jev's pick is below the bar: the recommended same-tier model wins.
#[tokio::test]
async fn an_unsure_judge_still_sends_low_opus_to_terra() {
    fails_over(
        at("anthropic", "claude-opus-5-5", "low"),
        at("openai", "gpt-6.1-terra", "high"),
        "openai/gpt-6.2-sol#high",
        Judged {
            choice: "openai/gpt-6-luna",
            confidence: 0.1,
        },
    )
    .await;
}

/// Jev's pick is below the bar: Opus at low thinking wins over Sonnet.
#[tokio::test]
async fn an_unsure_judge_still_sends_terra_to_low_opus() {
    fails_over(
        at("openai", "gpt-6.1-terra", "high"),
        at("anthropic", "claude-opus-5-5", "low"),
        "anthropic/claude-opus-5-5#high",
        Judged {
            choice: "anthropic/claude-sonnet-5-5#high",
            confidence: 0.1,
        },
    )
    .await;
}
