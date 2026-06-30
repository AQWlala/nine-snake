//! Swarm integration test: pipeline validation + output contract.
//!
//! Validates the v2.0 swarm: single-agent and empty-pipeline tasks
//! execute gracefully (they no longer return errors — the orchestrator
//! falls back to default behavior). A real LLM round-trip is out of
//! scope for the integration test (covered by unit tests).

use nine_snake_lib::llm::LlmGateway;
use nine_snake_lib::llm::OllamaClient;

use nine_snake_lib::swarm::orchestrator::{SwarmOrchestrator, SwarmTask};

#[tokio::test]
async fn swarm_single_agent_by_kind_executes() {
    let client = std::sync::Arc::new(OllamaClient::new("http://127.0.0.1:1"));
    let gw = std::sync::Arc::new(LlmGateway::new(client, "m", None, None, None));
    let orch = SwarmOrchestrator::new_without_memory(gw);
    let mut task = SwarmTask::new("hi");
    task.agents = vec!["Coder".to_string()];

    // v2.0: single-agent by kind dispatches 1 agent (mock LLM → fails).
    let res = orch.execute(task).await;
    assert!(res.is_ok(), "single-agent by kind should execute");
    let report = res.unwrap();
    assert_eq!(report.failure_count, 1, "exactly 1 agent dispatched");
}

#[tokio::test]
async fn swarm_empty_agents_falls_back_to_default_pool() {
    let client = std::sync::Arc::new(OllamaClient::new("http://127.0.0.1:1"));
    let gw = std::sync::Arc::new(LlmGateway::new(client, "m", None, None, None));
    let orch = SwarmOrchestrator::new_without_memory(gw);
    let mut task = SwarmTask::new("hi");
    task.agents = vec![];

    // v2.0: empty agents falls back to default agent_count (3).
    let res = orch.execute(task).await;
    assert!(res.is_ok(), "empty agents should fall back to default pool");
    let report = res.unwrap();
    assert_eq!(report.failure_count, 3, "default 3 agents dispatched");
}

#[tokio::test]
async fn swarm_canonical_pipeline_is_well_formed() {
    // We do not exercise the network: this test asserts that the
    // canonical task is constructed correctly.
    let client = std::sync::Arc::new(OllamaClient::new("http://127.0.0.1:1"));
    let gw = std::sync::Arc::new(LlmGateway::new(client, "m", None, None, None));
    let orch = SwarmOrchestrator::new_without_memory(gw);
    let task = SwarmTask::new("design a snake");
    assert!(task.agents.is_empty());
    assert_eq!(task.max_retries, 1);
    // The orchestrator is constructable; verify the pool is non-empty.
    assert_eq!(orch.list_agents().len(), 6);
}
