//! Swarm commands — execute, list agents, get agent.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::api::server::NineSnakeService;
use crate::commands::error::CommandError;
use crate::swarm::{OrchestrationReport, SwarmTask};
use crate::AppState;

/// Tauri command: dispatch a swarm task.
#[tauri::command]
#[instrument(skip(state, task), fields(otel.kind = "swarm_execute"))]
pub async fn swarm_execute(
    state: State<'_, AppState>,
    task: SwarmTask,
) -> Result<OrchestrationReport, CommandError> {
    // v1.1: Prompt injection scan before processing.
    let scan = crate::security::injection_guard::full_injection_scan(&task.description);
    if let Some(severity) = scan.max_severity {
        if severity >= crate::security::injection_guard::InjectionSeverity::Critical {
            tracing::warn!(
                target: "nine_snake.cmd",
                hits = scan.injection_hits.len(),
                leaks = scan.credential_leaks.len(),
                "blocked critical injection / credential leak in swarm_execute"
            );
            return Err(CommandError::validation("swarm_execute").with_details(
                "输入包含潜在的安全风险（注入攻击或凭证泄露），已被拦截".to_string(),
            ));
        }
        if !scan.safe {
            tracing::warn!(
                target: "nine_snake.cmd",
                severity = %severity,
                "non-critical injection warning in swarm_execute"
            );
        }
    }

    let report = state
        .swarm_execute(task)
        .await
        .map_err(|e| CommandError::swarm("swarm_execute", &e))?;
    crate::metrics::global().record_swarm();
    Ok(report)
}

/// v0.3: list the available swarm agents as `(kind, name, system, description)`.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "swarm_list_agents"))]
pub async fn swarm_list_agents(
    state: State<'_, AppState>,
) -> Result<Vec<(String, String, String, String)>, CommandError> {
    Ok(state.swarm.list_agents())
}

/// v0.3: fetch a single swarm agent by kind.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "swarm_get_agent"))]
pub async fn swarm_get_agent(
    state: State<'_, AppState>,
    kind: String,
) -> Result<Option<SwarmAgentInfo>, CommandError> {
    Ok(state.swarm.get_agent(&kind).map(|a| SwarmAgentInfo {
        name: a.name,
        system_prompt: a.system_prompt,
        description: a.description,
    }))
}

/// v0.3: agent descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmAgentInfo {
    pub name: String,
    pub system_prompt: String,
    pub description: String,
}
