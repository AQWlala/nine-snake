//! Skill commands — CRUD, import, marketplace, audit.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::skills::types as skill_types;
use crate::AppState;

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "skill_create"))]
pub async fn skill_create(
    state: State<'_, AppState>,
    request: skill_types::CreateSkillRequest,
) -> Result<skill_types::Skill, CommandError> {
    state
        .skills
        .create_skill(request)
        .map_err(|e| CommandError::db("skill_create", &e))
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "skill_use"))]
pub async fn skill_use(
    state: State<'_, AppState>,
    request: skill_types::UseSkillRequest,
) -> Result<skill_types::SkillResult, CommandError> {
    // v1.1: Prompt injection scan on skill input.
    let input_text = format!("{:?}", request);
    let scan = crate::security::injection_guard::full_injection_scan(&input_text);
    if let Some(severity) = scan.max_severity {
        if severity >= crate::security::injection_guard::InjectionSeverity::Critical {
            tracing::warn!(
                target: "nine_snake.cmd",
                "blocked critical injection / credential leak in skill_use"
            );
            return Err(CommandError::validation("skill_use").with_details(
                "输入包含潜在的安全风险（注入攻击或凭证泄露），已被拦截".to_string(),
            ));
        }
    }

    state
        .skills
        .use_skill(request)
        .await
        .map_err(|e| CommandError::internal("skill_use", &e))
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "skill_rate"))]
pub async fn skill_rate(
    state: State<'_, AppState>,
    request: skill_types::RateSkillRequest,
) -> Result<skill_types::Skill, CommandError> {
    state
        .skills
        .rate_skill(request)
        .map_err(|e| CommandError::db("skill_rate", &e))
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "skill_list"))]
pub async fn skill_list(
    state: State<'_, AppState>,
    request: skill_types::ListSkillsRequest,
) -> Result<Vec<skill_types::Skill>, CommandError> {
    state
        .skills
        .list_skills(request)
        .map_err(|e| CommandError::db("skill_list", &e))
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "skill_search"))]
pub async fn skill_search(
    state: State<'_, AppState>,
    request: skill_types::SkillSearchRequest,
) -> Result<Vec<skill_types::Skill>, CommandError> {
    state
        .skills
        .search_skills(request)
        .map_err(|e| CommandError::db("skill_search", &e))
}

/// Stub: import skill from external registry (v1.2 eco compatibility).
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "skill_import"))]
pub async fn skill_import(
    state: State<'_, AppState>,
    source: String,
    identifier: String,
) -> Result<crate::skills::importer::ImportResult, CommandError> {
    let source = match source.as_str() {
        "agentskills" => crate::skills::importer::SkillSource::AgentskillsIo,
        "clawhub" => crate::skills::importer::SkillSource::ClawHub,
        "teamskillshub" => crate::skills::importer::SkillSource::TeamSkillsHub,
        other => {
            return Err(CommandError::validation("skill_import")
                .with_details(format!("unknown source: {other}")))
        }
    };
    let importer = crate::skills::importer::SkillImporter::new(state.skills.store().clone());
    let result = match source {
        crate::skills::importer::SkillSource::AgentskillsIo => {
            importer.import_from_url(&identifier).await
        }
        crate::skills::importer::SkillSource::ClawHub => {
            importer.import_from_clawhub(&identifier).await
        }
        crate::skills::importer::SkillSource::TeamSkillsHub => {
            importer.import_from_teamskillshub(&identifier).await
        }
    };
    if result.success {
        Ok(result)
    } else {
        Err(CommandError::internal(
            "skill_import",
            &anyhow::anyhow!("import failed"),
        ))
    }
}

// -----------------------------------------------------------------------
// v1.3 P2-7: skill marketplace.
// -----------------------------------------------------------------------

/// Search the skill marketplace.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_search"))]
pub async fn marketplace_search(
    state: State<'_, AppState>,
    query: crate::skills::marketplace::MarketplaceQuery,
) -> Result<crate::skills::marketplace::MarketplaceResponse, CommandError> {
    state
        .marketplace
        .search(&query)
        .map_err(|e| CommandError::internal("marketplace_search", &e))
}

/// Quick search — top 10 results for autocomplete.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_quick_search"))]
pub async fn marketplace_quick_search(
    state: State<'_, AppState>,
    text: String,
) -> Result<crate::skills::marketplace::MarketplaceResponse, CommandError> {
    let q = crate::skills::marketplace::MarketplaceQuery {
        text: Some(text),
        limit: 10,
        ..Default::default()
    };
    state
        .marketplace
        .search(&q)
        .map_err(|e| CommandError::internal("marketplace_quick_search", &e))
}

/// One-click install from remote registry.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_install"))]
pub async fn marketplace_install(
    state: State<'_, AppState>,
    source: String,
    identifier: String,
) -> Result<crate::skills::marketplace::SkillEntry, CommandError> {
    state
        .marketplace
        .install(&source, &identifier)
        .map_err(|e| CommandError::internal("marketplace_install", &e))
}

/// Check for skill updates.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_check_updates"))]
pub async fn marketplace_check_updates(
    state: State<'_, AppState>,
) -> Result<Vec<crate::skills::marketplace::UpdateInfo>, CommandError> {
    Ok(state.marketplace.check_updates())
}

/// Refresh marketplace index.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_refresh"))]
pub async fn marketplace_refresh(
    state: State<'_, AppState>,
) -> Result<crate::skills::marketplace::MarketplaceStats, CommandError> {
    state
        .marketplace
        .refresh()
        .map_err(|e| CommandError::internal("marketplace_refresh", &e))
}

/// Get marketplace stats.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_stats"))]
pub async fn marketplace_stats(
    state: State<'_, AppState>,
) -> Result<crate::skills::marketplace::MarketplaceStats, CommandError> {
    Ok(state.marketplace.stats())
}

/// Get all tags with frequencies.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_tags"))]
pub async fn marketplace_tags(
    state: State<'_, AppState>,
) -> Result<Vec<(String, usize)>, CommandError> {
    Ok(state.marketplace.all_tags())
}

/// Generate publish manifest for a skill.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "marketplace_generate_manifest"))]
pub async fn marketplace_generate_manifest(
    state: State<'_, AppState>,
    skill_id: String,
) -> Result<crate::skills::marketplace::PublishManifest, CommandError> {
    state
        .marketplace
        .generate_manifest(&skill_id)
        .map_err(|e| CommandError::internal("marketplace_generate_manifest", &e))
}

// -----------------------------------------------------------------------
// v1.3: Skill audit log commands
// -----------------------------------------------------------------------

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "skill_audit_list"))]
pub async fn skill_audit_list(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<crate::skills::audit::SkillAuditEntry>, CommandError> {
    let logger = state.skill_audit_logger.clone();
    tokio::task::spawn_blocking(move || {
        logger
            .list(limit.unwrap_or(50))
            .map_err(|e| CommandError::db("skill_audit_list", &e))
    })
    .await
    .map_err(|e| CommandError::internal("skill_audit_list", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "skill_audit_list_for_skill"))]
pub async fn skill_audit_list_for_skill(
    state: State<'_, AppState>,
    skill_id: String,
    limit: Option<usize>,
) -> Result<Vec<crate::skills::audit::SkillAuditEntry>, CommandError> {
    let logger = state.skill_audit_logger.clone();
    tokio::task::spawn_blocking(move || {
        logger
            .list_for_skill(&skill_id, limit.unwrap_or(50))
            .map_err(|e| CommandError::db("skill_audit_list_for_skill", &e))
    })
    .await
    .map_err(|e| CommandError::internal("skill_audit_list_for_skill", &anyhow::anyhow!("{e}")))?
}
