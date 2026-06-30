//! Integration tests that require a running Ollama server.
//!
//! These tests are gated behind both `#[ignore]` (so they don't run
//! on a default `cargo test`) **and** the `OLLAMA_TEST=1` env var (so
//! a developer can opt-in to running them locally with
//! `OLLAMA_TEST=1 cargo test --test integration -- --ignored --nocapture`).
//!
//! To run these tests:
//! 1. Install Ollama: <https://ollama.com/download>
//! 2. Pull a chat model:    `ollama pull qwen2.5:3b`
//! 3. Pull an embed model:  `ollama pull nomic-embed-text`
//! 4. Run:  `OLLAMA_TEST=1 cargo test --test integration llm -- --ignored`
//!
//! The tests are designed to be best-effort: a model that doesn't
//! exist will fail with a clear "model not found" error and the test
//! will print that error. The success path validates that the LLM
//! returns a non-empty string within a reasonable timeout.

use std::time::Duration;

use nine_snake_lib::llm::{LlmGateway, OllamaClient};

const OLLAMA_URL: &str = "http://127.0.0.1:11434";
const CHAT_MODEL: &str = "qwen2.5:3b";
const EMBED_MODEL: &str = "nomic-embed-text";

fn ollama_enabled() -> bool {
    std::env::var("OLLAMA_TEST")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn make_gateway() -> LlmGateway {
    let client = std::sync::Arc::new(OllamaClient::new(OLLAMA_URL));
    LlmGateway::new(client, CHAT_MODEL, None, None, None)
}

#[tokio::test]
#[ignore = "requires OLLAMA_TEST=1 and a running ollama server"]
async fn real_chat_against_ollama() {
    if !ollama_enabled() {
        eprintln!("OLLAMA_TEST=1 not set; skipping real chat test");
        return;
    }
    let gw = make_gateway();
    let resp = tokio::time::timeout(
        Duration::from_secs(60),
        gw.chat(vec![nine_snake_lib::llm::ChatMessage::user(
            "Reply with exactly the word 'pong' and nothing else.",
        )]),
    )
    .await
    .expect("chat timeout (60s)")
    .expect("chat failed");

    assert!(
        !resp.message.content.is_empty(),
        "chat returned empty content"
    );
    assert!(
        resp.message.content.to_lowercase().contains("pong"),
        "expected 'pong' in reply, got: {:?}",
        resp.message.content
    );
}

#[tokio::test]
#[ignore = "requires OLLAMA_TEST=1 and a running ollama server"]
async fn real_embed_against_ollama() {
    if !ollama_enabled() {
        eprintln!("OLLAMA_TEST=1 not set; skipping real embed test");
        return;
    }
    let client = std::sync::Arc::new(OllamaClient::new(OLLAMA_URL));
    let embedder =
        nine_snake_lib::memory::embedder::Embedder::new((*client).clone(), EMBED_MODEL, 768);
    let vec = tokio::time::timeout(Duration::from_secs(60), embedder.embed("hello world"))
        .await
        .expect("embed timeout (60s)")
        .expect("embed failed");

    assert_eq!(
        vec.len(),
        768,
        "expected 768-dim embedding, got {}",
        vec.len()
    );
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 0.5,
        "embedding norm should be near 1.0, got {norm}"
    );
}

#[tokio::test]
#[ignore = "requires OLLAMA_TEST=1 and a running ollama server"]
async fn real_swarm_run_against_ollama() {
    if !ollama_enabled() {
        eprintln!("OLLAMA_TEST=1 not set; skipping real swarm test");
        return;
    }
    let gw = std::sync::Arc::new(make_gateway());
    let orch = nine_snake_lib::swarm::SwarmOrchestrator::new_without_memory(gw);
    let report = tokio::time::timeout(
        Duration::from_secs(180),
        orch.execute(nine_snake_lib::swarm::SwarmTask {
            description: "Say pong in one short sentence.".to_string(),
            agent_count: 3,
            agents: vec![],
            max_retries: 0,
        }),
    )
    .await
    .expect("swarm timeout (180s)")
    .expect("swarm failed");

    assert!(!report.outputs.is_empty(), "swarm produced no outputs");
    assert!(
        report.outputs.iter().any(|o| !o.body.trim().is_empty()),
        "at least one agent should have produced non-empty output"
    );
}
