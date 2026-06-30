use anyhow::Result;
use async_trait::async_trait;

use super::config::McpTransportType;

#[derive(Debug, Clone)]
pub enum McpTransport {
    Stdio { command: String },
    Http { url: String },
}

impl McpTransport {
    pub fn from_config(
        transport_type: &McpTransportType,
        command: Option<&str>,
        url: Option<&str>,
    ) -> Result<Self> {
        match transport_type {
            McpTransportType::Stdio => {
                let cmd =
                    command.ok_or_else(|| anyhow::anyhow!("stdio transport requires a command"))?;
                Ok(McpTransport::Stdio {
                    command: cmd.to_string(),
                })
            }
            McpTransportType::Http => {
                let u = url.ok_or_else(|| anyhow::anyhow!("http transport requires a url"))?;
                Ok(McpTransport::Http { url: u.to_string() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_transport_from_config() {
        let t = McpTransport::from_config(&McpTransportType::Stdio, Some("npx"), None).unwrap();
        match t {
            McpTransport::Stdio { command } => assert_eq!(command, "npx"),
            _ => panic!("expected Stdio"),
        }
    }
}
