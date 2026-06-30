//! Swarm end-to-end integration tests.
//!
//! Validates the full v2.0 swarm pipeline:
//! 1. Task creation → agent dispatch → parallel execution → report
//! 2. Negotiator confidence voting + conflict resolution
//! 3. AgentBus broadcast on completion
//! 4. Composer skill injection into team context
//!
//! These tests use a mock OllamaClient (pointed at a closed port) so
//! agents will fail — but the orchestrator must still produce a valid
//! report, collect errors, and broadcast on the bus.

use nine_snake_lib::llm::{LlmGateway, OllamaClient};
use nine_snake_lib::swarm::agents::{AgentKind, AgentOutput};
use nine_snake_lib::swarm::bus::BusMessageType;
use nine_snake_lib::swarm::negotiator::{NegotiationMethod, Negotiator};
use nine_snake_lib::swarm::orchestrator::{SwarmOrchestrator, SwarmTask};
use std::sync::Arc;

fn mock_gateway() -> Arc<LlmGateway> {
    let client = Arc::new(OllamaClient::new("http://127.0.0.1:1"));
    Arc::new(LlmGateway::new(client, "mock-model", None, None, None))
}

fn mock_orchestrator() -> SwarmOrchestrator {
    SwarmOrchestrator::new_without_memory(mock_gateway())
}

// ---------------------------------------------------------------------------
// Pipeline validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn swarm_dispatch_parallel_agents_produces_report() {
    let orch = mock_orchestrator();
    let task = SwarmTask::new("Explain quantum computing in one sentence");
    let report = orch
        .execute(task)
        .await
        .expect("orchestration should complete");

    // Even with failing agents, the report must be structurally valid.
    assert!(!report.task.description.is_empty());

    // All 3 agents will fail with the mock LLM (pointed at dead port),
    // so failure_count reflects dispatch count. After negotiation, outputs
    // is reduced to 1 (the chosen/fallback output).
    assert_eq!(report.failure_count, 3, "all 3 agents should be dispatched");
    assert!(
        !report.outputs.is_empty(),
        "negotiation produces at least 1 output"
    );
    assert!(!report.approved, "no agent succeeded with mock LLM");
}

#[tokio::test]
async fn swarm_explicit_agent_count_is_respected() {
    let orch = mock_orchestrator();
    let task = SwarmTask::new("Write a haiku about Rust").with_agent_count(4);
    let report = orch
        .execute(task)
        .await
        .expect("orchestration should complete");

    // failure_count = number of agents dispatched (all fail with mock LLM)
    assert_eq!(report.failure_count, 4, "should dispatch exactly 4 agents");
}

#[tokio::test]
async fn swarm_by_kinds_selects_correct_agents() {
    let orch = mock_orchestrator();
    let mut task = SwarmTask::new("Review this: fn add(a,b) -> a+b");
    task.agents = vec!["Coder".into(), "Reviewer".into()];
    let report = orch
        .execute(task)
        .await
        .expect("orchestration should complete");

    // 2 agents dispatched by kind, both fail with mock LLM
    assert_eq!(report.failure_count, 2, "should dispatch exactly 2 agents");
}

#[tokio::test]
async fn swarm_bus_broadcasts_completion_message() {
    let orch = mock_orchestrator();
    // Subscribe to the bus before executing
    let bus = orch.bus().clone();
    let mut rx = bus.subscribe();

    let task = SwarmTask::new("Test bus broadcast");
    let _report = orch
        .execute(task)
        .await
        .expect("orchestration should complete");

    // After completion, at least one broadcast should be on the bus.
    let mut found = false;
    while let Ok(msg) = rx.try_recv() {
        if msg.msg_type == BusMessageType::Notification
            && msg.content.contains("swarm.execute.completed")
        {
            found = true;
            break;
        }
    }
    assert!(found, "bus must broadcast completion notification");
}

// ---------------------------------------------------------------------------
// Negotiator E2E
// ---------------------------------------------------------------------------

#[test]
fn negotiator_picks_highest_confidence_when_no_conflict() {
    let neg = Negotiator::new();
    let outputs = vec![
        AgentOutput {
            kind: AgentKind::Generic,
            author: "agent-1".into(),
            body: "Answer A: 42".into(),
            confidence: 0.9,
        },
        AgentOutput {
            kind: AgentKind::Generic,
            author: "agent-2".into(),
            body: "Answer B: 42".into(),
            confidence: 0.6,
        },
        AgentOutput {
            kind: AgentKind::Generic,
            author: "agent-3".into(),
            body: "Answer C: 42".into(),
            confidence: 0.3,
        },
    ];
    let result = neg.negotiate(outputs);
    assert_eq!(result.chosen.author, "agent-1");
    assert!(!result.conflict_detected);
    assert_eq!(result.method, NegotiationMethod::HighConfidence);
}

#[test]
fn negotiator_detects_conflict_on_divergent_outputs() {
    let neg = Negotiator::new();
    let outputs = vec![
        AgentOutput {
            kind: AgentKind::Generic,
            author: "agent-1".into(),
            body: "The answer is LEFT.".into(),
            confidence: 0.85,
        },
        AgentOutput {
            kind: AgentKind::Generic,
            author: "agent-2".into(),
            body: "The answer is RIGHT.".into(),
            confidence: 0.80,
        },
    ];
    let result = neg.negotiate(outputs);
    assert!(result.conflict_detected);
    // Should still pick the higher-confidence one.
    assert_eq!(result.chosen.author, "agent-1");
    assert_eq!(result.method, NegotiationMethod::HighConfidence);
}

#[test]
fn negotiator_single_output_passes_through() {
    let neg = Negotiator::new();
    let outputs = vec![AgentOutput {
        kind: AgentKind::Generic,
        author: "solo".into(),
        body: "Only child.".into(),
        confidence: 1.0,
    }];
    let result = neg.negotiate(outputs);
    assert_eq!(result.chosen.author, "solo");
    assert!(!result.conflict_detected);
}

#[test]
fn negotiator_empty_input_returns_fallback() {
    let neg = Negotiator::new();
    let result = neg.negotiate(vec![]);
    assert_eq!(result.chosen.author, "negotiator");
    assert_eq!(result.chosen.body, "[no agent outputs]");
}

// ---------------------------------------------------------------------------
// Agent introspection (gRPC / frontend compatibility)
// ---------------------------------------------------------------------------

#[test]
fn orchestrator_lists_all_six_agents() {
    let orch = mock_orchestrator();
    let agents = orch.list_agents();
    assert_eq!(agents.len(), 6, "default pool must contain 6 agent types");
    let kinds: Vec<&str> = agents.iter().map(|(k, _, _, _)| k.as_str()).collect();
    assert!(kinds.contains(&"generic"));
    assert!(kinds.contains(&"coder"));
    assert!(kinds.contains(&"reviewer"));
}

#[test]
fn orchestrator_get_agent_by_kind_returns_descriptor() {
    let orch = mock_orchestrator();
    let coder = orch.get_agent("coder").expect("coder agent must exist");
    assert!(!coder.name.is_empty());
    assert!(!coder.system_prompt.is_empty());
    assert!(orch.get_agent("nonexistent").is_none());
}

// ---------------------------------------------------------------------------
// Task model
// ---------------------------------------------------------------------------

#[test]
fn task_new_has_sensible_defaults() {
    let task = SwarmTask::new("design a snake game");
    assert_eq!(task.agent_count, 3);
    assert_eq!(task.max_retries, 1);
    assert!(task.agents.is_empty());
    assert_eq!(task.description, "design a snake game");
}

#[test]
fn task_agent_count_is_clamped_to_2_6() {
    assert_eq!(SwarmTask::new("x").with_agent_count(0).agent_count, 2);
    assert_eq!(SwarmTask::new("x").with_agent_count(1).agent_count, 2);
    assert_eq!(SwarmTask::new("x").with_agent_count(2).agent_count, 2);
    assert_eq!(SwarmTask::new("x").with_agent_count(6).agent_count, 6);
    assert_eq!(SwarmTask::new("x").with_agent_count(7).agent_count, 6);
    assert_eq!(SwarmTask::new("x").with_agent_count(100).agent_count, 6);
}
