//! v1.3 P1-3: 技能沙箱隔离 — 基于能力的权限模型。
//!
//! 当前技能执行通过 Python 子进程沙箱（见 `skills::engine::execute_shell`），
//! 具备进程隔离 + 网络阻断 + 内存限制。本模块在此之上增加**声明式能力**
//! 权限系统，为未来 WASM 沙箱（wasmtime）迁移奠定基础。
//!
//! ## 设计原则
//!
//! 1. **能力即权限** — 每个能力是一枚令牌，技能只有持有令牌才能执行对应操作。
//! 2. **最小权限** — 技能默认零权限，必须显式声明所需能力。
//! 3. **渐进增强** — 当前通过进程级限制实现；WASM 迁移后通过
//!    wasmtime 的 `Linker` + WASI 裁剪实现细粒度控制。
//!
//! ## 参考
//!
//! - IronClaw 的 WASM 能力模型（capability-based permissions）
//! - Hermes 的 Docker 沙箱模式
//! - WASI preview2 的 world-based 能力模型

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// 技能执行能力 — 一枚令牌代表一项操作权限。
///
/// 当前仅定义语义能力，具体限制在进程级执行。
/// WASM 迁移后每个能力对应一个 wasmtime `Linker` 注册项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 读取本地文件系统。
    FileRead,
    /// 写入本地文件系统。
    FileWrite,
    /// 发起网络请求（HTTP/HTTPS）。
    Network,
    /// 执行子进程（如 Python 解释器本身）。
    Subprocess,
    /// 访问系统环境变量（只读）。
    EnvRead,
    /// 读取剪贴板内容。
    ClipboardRead,
    /// 与 LLM 网关交互。
    LlmCall,
    /// 访问 SQLite 数据库（技能专属表）。
    DbAccess,
}

impl Capability {
    /// 所有可用能力列表。
    pub const ALL: &[Capability] = &[
        Capability::FileRead,
        Capability::FileWrite,
        Capability::Network,
        Capability::Subprocess,
        Capability::EnvRead,
        Capability::ClipboardRead,
        Capability::LlmCall,
        Capability::DbAccess,
    ];

    /// 能力的人类可读描述。
    pub fn description(&self) -> &'static str {
        match self {
            Capability::FileRead => "读取本地文件",
            Capability::FileWrite => "写入本地文件",
            Capability::Network => "发起网络请求",
            Capability::Subprocess => "执行子进程",
            Capability::EnvRead => "读取环境变量",
            Capability::ClipboardRead => "读取剪贴板",
            Capability::LlmCall => "调用 LLM 网关",
            Capability::DbAccess => "访问数据库",
        }
    }

    /// 能力的安全风险等级。
    pub fn risk_level(&self) -> RiskLevel {
        match self {
            Capability::LlmCall => RiskLevel::Low,
            Capability::EnvRead | Capability::ClipboardRead => RiskLevel::Medium,
            Capability::FileRead | Capability::DbAccess => RiskLevel::Medium,
            Capability::FileWrite | Capability::Network | Capability::Subprocess => RiskLevel::High,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Capability::FileRead => "file:read",
            Capability::FileWrite => "file:write",
            Capability::Network => "network",
            Capability::Subprocess => "subprocess",
            Capability::EnvRead => "env:read",
            Capability::ClipboardRead => "clipboard:read",
            Capability::LlmCall => "llm:call",
            Capability::DbAccess => "db:access",
        };
        write!(f, "{s}")
    }
}

/// 风险等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "低"),
            RiskLevel::Medium => write!(f, "中"),
            RiskLevel::High => write!(f, "高"),
        }
    }
}

/// 技能的能力清单 — 声明该技能需要哪些权限。
///
/// 嵌入在 `SKILL.md` 的 YAML front-matter 中：
/// ```yaml
/// capabilities:
///   - file:read
///   - llm:call
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// 已授予的能力集合。
    granted: HashSet<Capability>,
}

impl CapabilitySet {
    /// 创建一个空的能力集（默认：零权限）。
    pub fn new() -> Self {
        Self {
            granted: HashSet::new(),
        }
    }

    /// 创建一个仅有 LLM 调用权限的最小能力集。
    /// 适用于纯 LLM 驱动的技能（不执行代码）。
    pub fn llm_only() -> Self {
        let mut s = Self::new();
        s.grant(Capability::LlmCall);
        s
    }

    /// 创建一个完全信任的能力集（仅用于用户显式授权的场景）。
    pub fn full_trust() -> Self {
        let mut s = Self::new();
        for cap in Capability::ALL {
            s.grant(*cap);
        }
        s
    }

    /// 授予一项能力。
    pub fn grant(&mut self, cap: Capability) {
        self.granted.insert(cap);
    }

    /// 撤销一项能力。
    pub fn revoke(&mut self, cap: Capability) {
        self.granted.remove(&cap);
    }

    /// 检查是否持有指定能力。
    pub fn has(&self, cap: Capability) -> bool {
        self.granted.contains(&cap)
    }

    /// 检查是否持有所有指定能力。
    pub fn has_all(&self, caps: &[Capability]) -> bool {
        caps.iter().all(|c| self.has(*c))
    }

    /// 检查是否持有任意指定能力。
    pub fn has_any(&self, caps: &[Capability]) -> bool {
        caps.iter().any(|c| self.has(*c))
    }

    /// 返回所有已授予的能力。
    pub fn granted(&self) -> Vec<Capability> {
        let mut caps: Vec<_> = self.granted.iter().copied().collect();
        caps.sort_by_key(|c| c.risk_level() as u8);
        caps
    }

    /// 检查是否为空（零权限）。
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty()
    }
}

/// 沙箱执行策略 — 控制技能如何被执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SandboxPolicy {
    /// 严格模式：仅允许通过声明式能力授权的操作。
    /// 缺少的能力调用将返回错误而非静默通过。
    #[default]
    Strict,
    /// 宽松模式：允许能力列表之外的操作，但在日志中记录警告。
    /// 适用于用户信任的本地技能。
    Permissive,
    /// 仅 LLM 模式：完全禁止代码执行，技能仅作为 LLM 提示词模板。
    LlmOnly,
}

impl fmt::Display for SandboxPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxPolicy::Strict => write!(f, "strict"),
            SandboxPolicy::Permissive => write!(f, "permissive"),
            SandboxPolicy::LlmOnly => write!(f, "llm_only"),
        }
    }
}

/// 沙箱配置 — 一个技能的完整沙箱设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// 该技能的能力清单。
    pub capabilities: CapabilitySet,
    /// 执行策略。
    pub policy: SandboxPolicy,
    /// 技能执行超时（毫秒）。0 表示使用引擎默认值（5000ms）。
    pub timeout_ms: u64,
    /// 内存限制（字节）。0 表示使用引擎默认值（100MB）。
    pub mem_limit_bytes: u64,
    /// 是否允许直接访问文件系统（默认仅允许临时目录）。
    pub allow_filesystem: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            capabilities: CapabilitySet::llm_only(),
            policy: SandboxPolicy::Strict,
            timeout_ms: 0,
            mem_limit_bytes: 0,
            allow_filesystem: false,
        }
    }
}

/// 沙箱执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    /// 执行是否成功。
    pub success: bool,
    /// 标准输出（被截断）。
    pub stdout: String,
    /// 标准错误（被截断）。
    pub stderr: String,
    /// 执行耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 被拒绝的能力调用列表。
    pub denied_capabilities: Vec<Capability>,
    /// 沙箱策略。
    pub policy: SandboxPolicy,
}

/// 沙箱违规错误。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("能力不足：技能未持有 {0} 权限")]
    MissingCapability(Capability),

    #[error("沙箱策略冲突：当前为 {0} 模式，不允许代码执行")]
    PolicyConflict(SandboxPolicy),

    #[error("沙箱运行时错误：{0}")]
    Runtime(String),
}

#[cfg(feature = "wasm-sandbox")]
mod wasm_sandbox {
    use super::{Capability, CapabilitySet, SandboxResult};
    use anyhow::{anyhow, Result};
    use std::sync::Arc;
    use tracing::{info, warn};

    const DEFAULT_MAX_FUEL: u64 = 1_000_000;

    struct WasmState {
        capabilities: CapabilitySet,
    }

    pub struct WasmSandbox {
        engine: wasmtime::Engine,
        linker: wasmtime::Linker<WasmState>,
        capabilities: CapabilitySet,
        max_fuel: u64,
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct WasmSandboxConfig {
        pub capabilities: CapabilitySet,
        pub max_fuel: u64,
    }

    impl Default for WasmSandboxConfig {
        fn default() -> Self {
            Self {
                capabilities: CapabilitySet::llm_only(),
                max_fuel: DEFAULT_MAX_FUEL,
            }
        }
    }

    impl WasmSandbox {
        pub fn new(config: &WasmSandboxConfig) -> Result<Self> {
            let mut engine_config = wasmtime::Config::new();
            engine_config.consume_fuel(true);
            let engine = wasmtime::Engine::new(&engine_config)?;

            let mut linker = wasmtime::Linker::new(&engine);

            if config.capabilities.has(Capability::FileRead) {
                linker.func_wrap("env", "file_read", |_path: i32| -> i32 { -1 })?;
            }

            if config.capabilities.has(Capability::FileWrite) {
                linker.func_wrap("env", "file_write", |_path: i32, _data: i32| -> i32 { -1 })?;
            }

            if config.capabilities.has(Capability::Network) {
                linker.func_wrap("env", "http_fetch", |_url: i32| -> i32 { -1 })?;
            }

            Ok(Self {
                engine,
                linker,
                capabilities: config.capabilities.clone(),
                max_fuel: config.max_fuel,
            })
        }

        pub fn execute(&self, wasm_bytes: &[u8], func_name: &str) -> Result<SandboxResult> {
            let start = std::time::Instant::now();

            let module = wasmtime::Module::new(&self.engine, wasm_bytes)
                .map_err(|e| anyhow!("WASM module compilation failed: {e}"))?;

            let mut store = wasmtime::Store::new(
                &self.engine,
                WasmState {
                    capabilities: self.capabilities.clone(),
                },
            );

            store.set_fuel(self.max_fuel)?;

            let instance = self
                .linker
                .instantiate(&mut store, &module)
                .map_err(|e| anyhow!("WASM instantiation failed: {e}"))?;

            let func = instance
                .get_func(&mut store, func_name)
                .ok_or_else(|| anyhow!("exported function '{func_name}' not found"))?;

            let result = func.call(&mut store, &[], &mut []);

            let elapsed_ms = start.elapsed().as_millis() as u64;

            match result {
                Ok(_) => {
                    info!(target: "nine_snake.wasm", func = func_name, elapsed_ms, "WASM execution completed");
                    Ok(SandboxResult {
                        success: true,
                        stdout: String::new(),
                        stderr: String::new(),
                        elapsed_ms,
                        denied_capabilities: vec![],
                        policy: super::SandboxPolicy::Strict,
                    })
                }
                Err(e) => {
                    let msg = format!("{e}");
                    let out_of_fuel = msg.contains("all fuel consumed");
                    if out_of_fuel {
                        warn!(target: "nine_snake.wasm", func = func_name, max_fuel = self.max_fuel, "WASM execution ran out of fuel");
                    }
                    Ok(SandboxResult {
                        success: false,
                        stdout: String::new(),
                        stderr: msg,
                        elapsed_ms,
                        denied_capabilities: if out_of_fuel {
                            vec![Capability::Subprocess]
                        } else {
                            vec![]
                        },
                        policy: super::SandboxPolicy::Strict,
                    })
                }
            }
        }

        pub fn engine(&self) -> &wasmtime::Engine {
            &self.engine
        }

        pub fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
    }
}

#[cfg(feature = "wasm-sandbox")]
pub use wasm_sandbox::{WasmSandbox, WasmSandboxConfig};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_has_no_capabilities() {
        let caps = CapabilitySet::new();
        assert!(caps.is_empty());
        assert!(!caps.has(Capability::FileRead));
        assert!(!caps.has_any(&[Capability::Network, Capability::LlmCall]));
    }

    #[test]
    fn llm_only_has_llm_call() {
        let caps = CapabilitySet::llm_only();
        assert!(caps.has(Capability::LlmCall));
        assert!(!caps.has(Capability::FileRead));
        assert!(!caps.has(Capability::Network));
    }

    #[test]
    fn full_trust_has_all() {
        let caps = CapabilitySet::full_trust();
        for cap in Capability::ALL {
            assert!(caps.has(*cap), "full_trust must have {cap:?}");
        }
    }

    #[test]
    fn grant_and_revoke() {
        let mut caps = CapabilitySet::new();
        caps.grant(Capability::FileRead);
        assert!(caps.has(Capability::FileRead));
        caps.revoke(Capability::FileRead);
        assert!(!caps.has(Capability::FileRead));
    }

    #[test]
    fn risk_level_ordering() {
        let mut caps = CapabilitySet::new();
        caps.grant(Capability::Subprocess); // High
        caps.grant(Capability::LlmCall); // Low
        caps.grant(Capability::FileRead); // Medium
        let granted = caps.granted();
        // Low -> Medium -> High
        assert_eq!(granted[0], Capability::LlmCall);
        assert_eq!(granted[1], Capability::FileRead);
        assert_eq!(granted[2], Capability::Subprocess);
    }

    #[test]
    fn sandbox_config_defaults_to_strict_llm_only() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.policy, SandboxPolicy::Strict);
        assert!(cfg.capabilities.has(Capability::LlmCall));
        assert!(!cfg.capabilities.has(Capability::FileRead));
        assert!(!cfg.allow_filesystem);
    }
}
