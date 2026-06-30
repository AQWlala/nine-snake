//! v1.1 P0-2: Shell execution as a `Tool`.

use super::{Tool, ToolOutput};
use crate::os::ShellExecutor;
use anyhow::Result;

pub struct ShellTool {
    executor: ShellExecutor,
}

impl ShellTool {
    pub fn new(executor: ShellExecutor) -> Self {
        Self { executor }
    }
}

impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell_exec"
    }

    fn description(&self) -> &str {
        "Execute a whitelisted shell command. Args: argv (array of strings), cwd (optional working directory)."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command and arguments, e.g. [\"ls\", \"-la\"]"
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory"
                }
            },
            "required": ["argv"]
        })
    }

    fn call(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let argv: Vec<String> = serde_json::from_value(args["argv"].clone())
            .map_err(|e| anyhow::anyhow!("invalid argv: {e}"))?;
        let cwd = args["cwd"].as_str().map(std::path::Path::new);

        // Execute the command synchronously using block_in_place since
        // ShellExecutor::exec is async.
        let output = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.executor.exec(argv.clone(), cwd))
        });

        match output {
            Ok(o) => Ok(ToolOutput {
                success: o.exit_code == 0,
                result: o.stdout,
                error: if o.exit_code != 0 {
                    Some(o.stderr)
                } else {
                    None
                },
            }),
            Err(e) => Ok(ToolOutput {
                success: false,
                result: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_tool_name_and_description() {
        let ex = ShellExecutor::new();
        let tool = ShellTool::new(ex);
        assert_eq!(tool.name(), "shell_exec");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn shell_tool_schema_has_argv() {
        let ex = ShellExecutor::new();
        let tool = ShellTool::new(ex);
        let schema = tool.schema();
        assert_eq!(schema["properties"]["argv"]["type"], "array");
    }
}
