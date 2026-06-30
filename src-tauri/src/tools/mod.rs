//! v1.1 P0-2: Tool abstraction layer.
//!
//! Provides a uniform `Tool` trait that any capability (shell execution,
//! file read, web search, skill execution) can implement.  The
//! `ToolRegistry` maintains the live catalog and lets the swarm orchestrator
//! enumerate available tools for inclusion in the LLM system prompt.

pub mod shell_tool;
use anyhow::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Input to a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInput {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// Output from a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub success: bool,
    pub result: String,
    pub error: Option<String>,
}

/// A callable tool.  Implementors must be `Send + Sync` so the
/// registry can be shared across async tasks.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn call(&self, arguments: serde_json::Value) -> Result<ToolOutput>;
}

/// Thread-safe tool registry.
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool.  If a tool of the same name exists, it is replaced.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.tools.write().insert(tool.name().to_string(), tool);
    }

    /// Returns a registered tool by name, if found.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().get(name).cloned()
    }

    /// Lists all registered tools as `(name, description, schema)` tuples.
    pub fn list_all(&self) -> Vec<(String, String, serde_json::Value)> {
        self.tools
            .read()
            .iter()
            .map(|(k, t)| (k.clone(), t.description().to_string(), t.schema()))
            .collect()
    }

    /// Invokes a tool by name with the given arguments.
    pub fn invoke(&self, input: ToolInput) -> Result<ToolOutput> {
        let tool = self
            .get(&input.tool_name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", input.tool_name))?;
        tool.call(input.arguments)
    }

    pub fn register_mcp_tools(&self, server_name: &str, tools: Vec<Arc<dyn Tool>>) {
        let mut map = self.tools.write();
        for tool in tools {
            let prefixed = format!("mcp_{server_name}_{}", tool.name());
            map.insert(prefixed, tool);
        }
    }

    pub fn unregister_server(&self, server_name: &str) {
        let prefix = format!("mcp_{server_name}_");
        let mut map = self.tools.write();
        map.retain(|k, _| !k.starts_with(&prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "A dummy tool for testing"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn call(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                success: true,
                result: "ok".to_string(),
                error: None,
            })
        }
    }

    #[test]
    fn registry_register_and_get() {
        let reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn registry_list_all() {
        let reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        let all = reg.list_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "dummy");
    }

    #[test]
    fn registry_invoke_success() {
        let reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool));
        let out = reg
            .invoke(ToolInput {
                tool_name: "dummy".to_string(),
                arguments: serde_json::json!({}),
            })
            .unwrap();
        assert!(out.success);
        assert_eq!(out.result, "ok");
    }

    #[test]
    fn registry_invoke_unknown_tool() {
        let reg = ToolRegistry::new();
        let err = reg
            .invoke(ToolInput {
                tool_name: "unknown".to_string(),
                arguments: serde_json::json!({}),
            })
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }
}
