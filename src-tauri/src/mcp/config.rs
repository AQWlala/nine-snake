use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport_type: McpTransportType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tool_filter: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportType {
    Stdio,
    Http,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_serializes_roundtrip() {
        let cfg = McpServerConfig {
            name: "test-server".to_string(),
            transport_type: McpTransportType::Stdio,
            command: Some("npx".to_string()),
            url: None,
            enabled: true,
            tool_filter: vec![],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-server");
        assert_eq!(parsed.transport_type, McpTransportType::Stdio);
    }
}
