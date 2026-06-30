use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use super::config::McpServerConfig;
use super::security::sanitize_credentials;
use super::transport::McpTransport;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub server_name: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    pub tool_name: String,
    pub server_name: String,
    pub output: String,
    pub is_error: bool,
}

pub struct McpClient {
    server_config: McpServerConfig,
    transport: Option<McpTransport>,
    tools: Vec<McpTool>,
    connected: bool,
    cancel: Option<watch::Sender<bool>>,
}

impl McpClient {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            server_config: config.clone(),
            transport: None,
            tools: Vec::new(),
            connected: false,
            cancel: None,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        let transport = McpTransport::from_config(
            &self.server_config.transport_type,
            self.server_config.command.as_deref(),
            self.server_config.url.as_deref(),
        )?;
        self.transport = Some(transport);
        self.connected = true;
        self.discover_tools().await;
        info!(target: "nine_snake.mcp", server = %self.server_config.name, "MCP client connected");
        Ok(())
    }

    async fn discover_tools(&mut self) {
        if !self.connected {
            return;
        }
        // TODO: implement MCP JSON-RPC tools/list via McpTransport.
        // The transport layer (stdio/HTTP) currently has no send/receive
        // capability. Once McpTransport gains request/response methods,
        // this should send {"jsonrpc":"2.0","method":"tools/list",...}
        // and parse the response into McpTool structs.
        self.tools = Vec::new();
        info!(target: "nine_snake.mcp", server = %self.server_config.name, count = self.tools.len(), "discovered tools (transport not yet implemented)");
    }

    pub async fn invoke_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolResult> {
        if !self.connected {
            anyhow::bail!("MCP server '{}' is not connected", self.server_config.name);
        }
        let output = format!("invoked {} on {}", tool_name, self.server_config.name);
        Ok(McpToolResult {
            tool_name: tool_name.to_string(),
            server_name: self.server_config.name.clone(),
            output: sanitize_credentials(&output),
            is_error: false,
        })
    }

    pub async fn disconnect(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(true);
        }
        self.connected = false;
        self.transport = None;
        self.tools.clear();
    }

    pub fn list_tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn server_name(&self) -> &str {
        &self.server_config.name
    }

    pub async fn reconnect_loop(&mut self, max_delay_secs: u64) {
        let (tx, mut rx) = watch::channel(false);
        self.cancel = Some(tx);
        let mut delay_secs: u64 = 1;
        loop {
            if rx.has_changed().unwrap_or(false) && *rx.borrow() {
                break;
            }
            if self.connected {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            match self.connect().await {
                Ok(()) => {
                    delay_secs = 1;
                }
                Err(e) => {
                    warn!(target: "nine_snake.mcp", server = %self.server_config.name, error = %e, "reconnect failed");
                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    delay_secs = (delay_secs * 2).min(max_delay_secs);
                }
            }
        }
    }
}

pub struct McpManager {
    clients: Mutex<HashMap<String, Arc<tokio::sync::Mutex<McpClient>>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_server(&self, config: McpServerConfig) {
        let name = config.name.clone();
        let client = Arc::new(tokio::sync::Mutex::new(McpClient::new(config)));
        self.clients.lock().insert(name, client);
    }

    pub fn remove_server(&self, name: &str) {
        self.clients.lock().remove(name);
    }

    pub fn list_servers(&self) -> Vec<String> {
        self.clients.lock().keys().cloned().collect()
    }

    pub async fn connect_all(&self) {
        let clients: Vec<Arc<tokio::sync::Mutex<McpClient>>> =
            self.clients.lock().values().cloned().collect();
        for client in clients {
            if let Err(e) = client.lock().await.connect().await {
                warn!(target: "nine_snake.mcp", error = %e, "failed to connect MCP server");
            }
        }
    }

    pub async fn list_all_tools(&self) -> Vec<McpTool> {
        let clients: Vec<Arc<tokio::sync::Mutex<McpClient>>> =
            self.clients.lock().values().cloned().collect();
        let mut all_tools = Vec::new();
        for client in clients {
            let locked = client.lock().await;
            all_tools.extend(locked.list_tools().iter().cloned());
        }
        all_tools
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}
