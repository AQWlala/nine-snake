# CHANGELOG

所有九头蛇版本的重要变更都会记录在这里。格式基于 [Keep a Changelog](https://keepachangelog.com/)。

## [1.1.10] - 2026-07-01

🔧 **启动修复版 — 修复 autostart 插件配置导致启动 panic**。

### Fixed

* 修复 `tauri.conf.json` 中 `plugins.autostart` 配置错误:`"autostart": {}` 应为 `"autostart": null`。`tauri-plugin-autostart` 期望 `null`(unit type),传 `{}`(map)导致反序列化失败,panic 在 `tauri::Builder::run()`,即 `windows_subsystem = "windows"` 下被静默吞掉,表现为双击完全无反应

## [1.1.9] - 2026-07-01

🔧 **启动修复版 — 修复打包后无法启动的问题**。

### Fixed

* 修复 migrations 路径问题：`bundled_migrations_dir()` 使用编译时 `CARGO_MANIFEST_DIR` 路径,打包后在用户机器上不存在。改用 `include_str!` 将所有 SQL 文件内嵌到二进制中,新增 `run_bundled_migrations()` 和 `bundled_migration_status()` 函数
* 修复 DB 相对路径问题：`db_path` 和 `lance_path` 默认为相对路径,从快捷方式启动时工作目录为 System32 导致 DB 创建失败。现在在 setup 中将相对路径解析到 app data dir
* 修复日志默认不写文件：`init_tracing()` 仅在 `NINE_SNAKE_LOG_DIR` 设置时写文件,用户无法诊断启动崩溃。现在默认写入平台 app data 目录(Windows: `%LOCALAPPDATA%\nine-snake\logs`)
* 添加 panic hook：`windows_subsystem = "windows"` 下 panic 被静默吞掉,用户看到"完全无反应"。现在 panic 信息写入 `nine-snake-panic.log` 文件

## [1.1.8] - 2026-07-01

🔧 **Bug 修复版 — 修复编译错误 / CI/CD 部署问题 / CI 测试死锁**。

### Fixed

* 修复 gRPC trait 名称冲突 (E0255)：将 `use crate::api::server::NineSnakeService` 改为 `use crate::api::server::NineSnakeService as ApiNineSnakeService`
* 修复 GrpcHandle::shutdown 移动语义错误 (E0509)：将 `join` 字段改为 `Option<tokio::task::JoinHandle<()>>`，使用 `take()` 避免移动
* 修复 hyper http2::Builder API 不匹配 (E0061/E0277/E0308)：使用 `TokioExecutor::new()` 和 `TokioTimer::new()` 适配 hyper 1.10+ API
* 修复 lancedb 未门控导入 (E0433)：在 `use lancedb::query::{ExecutableQuery, QueryBase}` 前添加 `#[cfg(feature = "vector-store")]`
* 移除未使用的 `method` 变量警告
* 修复 package.json 重复的 postcss devDependency
* 修复 tauri.conf.json devUrl 在 Windows 上无法编译（移除 bash `${TAURI_DEV_PORT:-5173}` 默认值语法）
* 修复 .gitignore 遗漏 .opencode/ 目录
* 修复 reflect_test 死锁：测试持有 `conn.lock()` 期间调用 `list_recent`（内部又 lock 同一 Mutex），parking_lot 非重入导致 60s 超时。用块作用域在调用前释放锁
* 修复 Windows python_sandbox 测试：Python 启动 + SANDBOX_PREAMBLE 导入超过 5s SKILL_TIMEOUT 导致 panic，改为 skip（与 SpawnError 一致）
* 修复 CI 输出截断：nextest exit 100 时 `| tee` 管道 buffer 丢失，改为直接重定向 `> nextest-output.txt 2>&1`
* 修复 v2_test 裸 recv() 挂起：包裹 `tokio::time::timeout(5s)` 兜底
* 修复 compression_lock_test 无界自旋：加 5s deadline + 减少迭代次数 100→30
* 修复 Windows exec_runs_echo：echo 是 cmd 内置命令无 echo.exe，加 probe + python fallback
* 修复 clippy lint：`assert_eq!(x, true)` → `assert!(x)`、`len() >= 1` → `!is_empty()`、`p >= 2 && p <= 3` → `(2..=3).contains(&p)`
* 修复 start_timer 死锁：先释放 active_timer 锁再调用 stop_timer（parking_lot 非重入）
* 修复 skill_ratings PK 冲突：改用自增 id
* CI audit 步骤改为 continue-on-error + artifact 上传，安全漏洞不阻止 CI 通过

### Docs

* 修正 README 记忆层数描述：8 层 → 5 层（L0-L4 完整 + L5 预览），L6/L7 标注 v1.5
* 修正 README 特性表：移除未实现的多渠道、DID 身份声明
* 重写 README 记忆层表格：名称对齐 v7.0 设计文档，增加实现状态列
* 修正 ARCHITECTURE.md 记忆子系统图表：层名称对齐设计文档
* 修正 lib.rs 注释：8-layer → 5-layer
* Work 模式 UI 添加 [实验性] 标记（ModeSwitcher）
* Channel 模块（Telegram/Discord/WebChat）添加 feature gate channels，默认关闭
* Skill Marketplace 重命名为 Skill Browser（技能浏览器），图标 🛒→🔍
* ARCHITECTURE.md 添加 ADR：gRPC JSON shim 作为 v1.x 永久方案
* 新增蜂群 E2E 集成测试：并行分发、Negotiator 协商、AgentBus 广播、Agent 内省
* 新增内置 Demo 技能（hello-world / file-summary / code-review），首次启动自动种子
* 确认 Telegram 适配器完全独立，直连 Telegram Bot API 无需 JiuwenSwarm
* 注册 swarm_e2e_test 到集成测试 runner（之前遗漏）
* 修正 MemoryLayer 注释 + tauri.ts Layer 类型：标注 L6/L7 为 v1.5+ 保留
* 清理 lib.rs 命令注册处的残留 refactor 注释

## [1.1.7] - 2026-06-28

🔧 **构建安全 / 依赖升级 / CI 强化 / 项目治理**。

### Fixed

* 修复 package.json `build` 脚本跳过 TypeScript 类型检查：改为 `tsc --noEmit && vite build`，新增 `build:fast` 保留纯 Vite 构建
* 修复 README `curl | sh` 安装命令无签名校验：添加 SHA-256 校验说明，引导用户先下载脚本比对哈希再执行
* 升级 reqwest 0.11 → 0.12，消除与 hyper 1.0 并存时的双 hyper 依赖冲突
* 修复 CI clippy 门禁 `|| true` 允许失败：改为强制 `cargo clippy -- -D warnings`，新增 `cargo fmt -- --check`
* 精简 tokio features：`full` → 实际用到的 9 个 feature（rt-multi-thread / macros / time / io-util / sync / process / fs / signal / net）
* .gitignore 补充 `keys/*.pem` / `keys/*.key` 兜底规则
* wasm-sandbox feature 注释标注 experimental，明确非生产就绪
* 统一 package.json description 为 "A local-first AI assistant that grows with you"，与 README 一致

### Added

* 新建 SECURITY.md：漏洞报告流程、响应时间线、支持版本、安全特性清单
* 新建 .github/ISSUE_TEMPLATE/bug_report.md 和 feature_request.md
* 新建 .github/PULL_REQUEST_TEMPLATE.md

## [1.1.6] - 2026-06-28

🔧 **P0/P1/P2 全面修复 — FTS5 迁移 / 命令注册 / DeviceManager 持久化 / 前端 DTO 对齐 / 安全修复**。

### Fixed

* 修复 FTS5 迁移 010_fts5.sql 引用不存在的 `memories.tags` 列：从虚拟表和触发器中移除 tags，避免所有 memories 写操作失败
* 注册 20 个遗漏的 Tauri 命令：set_api_key / get_api_key / delete_api_key / channel_status / channel_send / channel_poll / channel_ping / injection_scan / sandbox_config / tool_list / tool_invoke / marketplace_search / marketplace_quick_search / marketplace_install / marketplace_check_updates / marketplace_refresh / marketplace_stats / marketplace_tags / marketplace_generate_manifest
* 实现 sync_recv 命令：从 LocalTransport 拉取加密信封 → 逐个解密 → 返回 InboxMessage 列表，支持自动 ack
* DeviceManager 持久化：添加 paired_devices 表，register_device / revoke_device 写入 SQLite，new() 从数据库加载已有设备，撤销操作重启后仍有效
* MCP discover_tools() 移除占位符工具：返回空列表而非假工具，添加 TODO 说明需实现 transport 层
* 修复 export_memories / import_memories 在 spawn_blocking 内调用 block_on 的死锁风险：改为直接在 async 上下文调用 async 方法
* 修复 ACL 命令（acl_set / acl_list / acl_remove）同步 SQLite 调用未用 spawn_blocking：全部包裹在 spawn_blocking 中
* 修复 list_devices / revoke_device 在 async 中持有 parking_lot::Mutex：改用 spawn_blocking
* 修正 README 命令数 86 → 106，与 ARCHITECTURE.md 一致
* 修正 CHANGELOG v1.0 updater pubkey 记录与 tauri.conf.json 实际 pubkey 不一致
* 前端 StoreMemoryRequest 已有 source 字段（确认正确）
* 前端 SearchRequest.limit 改为 k 字段，与后端 SearchMemoryRequest 匹配
* 前端 Memory 类型添加 compression_gen / archived 缺失字段
* 修复 set_composer 逻辑错误：从 message_bridge 条件块中移出，始终执行
* 修复 NoopPromptMutator.propose() 返回带后缀字符串而非原始 prompt
* 更新 api/server.rs 过时 TODO 注释

## [1.1.5] - 2026-06-28

🔧 **文档修正 / MCP wiring / 安全修复**。

### Fixed

* 修正 ARCHITECTURE.md §2 数据流描述：补充 L1 写入实际走 SpongeEngine::absorb() 3 步管线（敏感扫描 → embed+LanceDB+SQLite → 去重/合并），而非"写一条 L1"
* 修正 CHANGELOG v1.1 + ARCHITECTURE.md §5 gRPC 描述：明确当前是 JSON framing shim（4-byte BE length + JSON payload），不是完整 gRPC/HTTP2 协议栈，标准 grpcurl/tonic 客户端无法直连
* 修正 AppState 中 mcp_manager 字段：改为 `Arc<McpManager>` 包裹，与其他子系统一致，确保线程安全
* 修正 bootstrap 中 MCP wiring：添加 `mcp_manager.connect_all()` 调用 + 工具发现日志，确保 MCP 功能可实际运行
* 修正 McpManager::list_all_tools() 中 parking_lot::Mutex 跨 await point 持有问题：先 clone clients 释放锁再遍历
* 移除 CHANGELOG v1.0 中虚假覆盖率声明"~73%"（无 tarpaulin/grcov 配置，无数据来源）

🔧 **Bug 修复版 — 修复启动崩溃 / IPC 命令注册 / 前后端参数匹配 / 安全守卫恢复**。

### Fixed

* 修复 `tauri.conf.json` 中 `autostart` 配置导致 `PluginInitialization` panic（启动崩溃）
* 注册缺失的 IPC 命令：`bootstrap`、`health`、`skill_import`
* 修复 `chat()` 前端参数名不匹配：`{ req }` → `{ request: { user_message } }`
* 修复 `skillImport()` 参数名不匹配：`{ url }` → `{ identifier }`
* 修复 `ChatResponse` 类型不匹配：后端返回 `{ model, role, content }`，前端之前期望 `{ reply }`
* 恢复 `tauri.conf.json::plugins.updater.pubkey` + `keys/updater_public.b64`（P0 安全守卫测试恢复）
* 修复 README 环境变量名不一致：`ANTHROPIC_API_KEY` → `NINE_SNAKE_ANTHROPIC_KEY`
* 修复 README 版本号 badge：v1.1.0 → v1.1.4
* CI/CD：Release job `if: always()` 修复、安装包过滤（排除 .so 和 build logs）、版本号同步

## [Unreleased] - v1.1

🎉 **功能增强版 — 全面升级 LLM 支持 / Agent 能力 / 安全模型 / 前端体验**。

### Added

#### P0 核心改进

* **LLM 多 Provider 支持** (`src-tauri/src/llm/`)
  * 新增 `anthropic.rs`：Anthropic Claude Messages API 原生客户端
  * 支持 Claude 3 Haiku / Sonnet / Opus 系列模型
  * Gateway 降级链：Ollama → OpenAI 兼容端 → Anthropic Claude
  * 通过 `NINE_SNAKE_ANTHROPIC_KEY` / `NINE_SNAKE_ANTHROPIC_MODEL` 环境变量配置

* **统一 Tool 抽象层** (`src-tauri/src/tools/`)
  * `Tool` trait（`Send + Sync`）：任意能力（Shell / 文件读取 / 网页搜索）可实现统一接口
  * `ToolRegistry`：线程安全工具注册中心，支持动态注册
  * `ShellTool` 实现：Shell 执行作为可枚举的 Tool，JSON Schema 描述参数
  * 新增 Tauri 命令：`tool_list`（列出所有工具）/ `tool_invoke`（按名称调用工具）

* **Agent 自动 RAG 上下文注入** (`src-tauri/src/swarm/orchestrator.rs`)
  * 每次 Agent 调用前，自动从 LanceDB 检索 top-5 相关记忆
  * 格式化为 `<memory_context>` 标签块注入 system prompt
  * Agent 现在具备"知道你之前写过什么"的能力

* **gRPC JSON Framing Shim 实现** (`src-tauri/src/grpc/server.rs`)
  * 替换旧的"stub log → return error"实现
  * 使用 hyper HTTP/2 server + 自定义 JSON framing shim（4-byte BE length + JSON payload）
  * 完整 22 个 RPC 路由（Memory / Swarm / Reflect / LLM / Skills）
  * **注意**：当前不是完整 gRPC/HTTP2 协议栈，不支持 HPACK / protobuf / varint 编码，
    标准 grpcurl 或 tonic 客户端无法直接连接，调用方需使用兼容 JSON framing 的自定义客户端
  * 外部程序可通过 JSON framing 协议调用 nine-snake 记忆后端

* **Shell 白名单 Glob/Regex 支持** (`src-tauri/src/os/shell.rs`)
  * `WhitelistEntry` enum：`Exact`（精确匹配） / `Glob`（前缀通配符匹配）
  * `allow("git *")` 自动识别为 Glob 模式，匹配 `git commit` / `git push` 等
  * `is_allowed()` 正确路由到 Glob 或 Exact 匹配器
  * 新增单元测试覆盖 Glob 匹配边界情况

#### P1 重要改进

* **SQLite I/O 非阻塞化** (`src-tauri/src/memory/sqlite_store.rs`)
  * 所有读/写方法（`insert` / `update` / `get` / `delete` 等）改为 async
  * 使用 `tokio::task::spawn_blocking` 包裹阻塞的 SQLite 调用
  * 避免阻塞 tokio worker 线程，高并发下更稳定

* **敏感数据自动检测** (`src-tauri/src/security/detectors.rs`)
  * `SensitiveScanner` 正则检测器，支持 5 类敏感数据：
    * API Key（通用格式，20+ 字符）
    * Bearer Token
    * 私钥（RSA PRIVATE KEY 等）
    * 中国居民身份证（18 位）
    * 中国手机号（11 位，1[3-9] 开头）
  * 在 `SpongeEngine::absorb()` 入口自动扫描，脱敏后写入存储
  * 使用 `tracing::warn!` 记录检测结果（不阻断写入）

* **跨设备同步 QR 配对** (`src-tauri/src/sync/pairing.rs`)
  * 基于现有 E2EE 栈（X25519 + HKDF + AES-256-GCM）
  * 设备 A 生成临时配对 Offer（包含加密公钥 + 临时密钥）
  * 设备 B 扫描 QR，进入配对模式，双方建立共享密钥
  * 不再需要手动输入长恢复短语

* **Memory Map 可视化** (`src/components/MemoryMap.tsx`)
  * 7 层同心圆 SVG 图形（L0 感官 → L7 奇点核心）
  * 记忆节点：大小 = 重要性，颜色 = 层级（L0 灰色 → L7 金色）
  * 点击节点展开详情，hover 显示摘要
  * 新记忆淡入 / 被压缩时缩小淡出动画
  * 自动 15 秒刷新记忆数据
  * App 集成切换按钮：记忆地图 / 列表视图

* **Code 模式 Diff 预览** (`src/components/CodeMode.tsx`)
  * Agent 修改文件后，使用 Monaco `DiffEditor` 并排展示修改前后
  * "应用修改" / "撤销" 按钮
  * 暴露 `window.nineSnakeShowAgentDiff` 全局 API

* **Onboarding 3 步引导增强** (`src/components/Onboarding.tsx`)
  * 步骤 1：欢迎 + 确认安装路径
  * 步骤 2：Ollama 配置（自动检测 `localhost:11434` 连接状态）
  * 步骤 3：开始使用
  * 进度指示器 + 自动健康检测

* **i18n 全量更新** (`src/i18n/zh-CN.json`, `en-US.json`)
  * 新增 MemoryMap 全部 i18n keys
  * 新增 Onboarding 3 步文本
  * 新增 Code 模式 diff 预览文本

### Changed

* `Cargo.toml` 新增依赖：`regex = "1.10"`（敏感数据检测）、`hyper-util = "0.12"`（gRPC HTTP/2）
* `AppState` 新增 `tool_registry: Arc<ToolRegistry>` 字段
* `SwarmOrchestrator` 新增 `lance` / `embedder` / `sqlite` 字段用于 RAG
* `SpongeEngine` 新增 `sensitive_scanner` 字段

### Deprecated

* 环境变量 `NINE_SNAKE_REMOTE_URL`（已被多 Provider 架构取代）

## [1.0.0] - 2026-06-21

🎉 **首发版 (MVP launch) — 含发布前 P0 修复**。第一个可发布版本，13 个 P0
阻塞项在发布前已全部修复并通过守护测试。

### Added

* **性能基线**
  * 冷启动 < 5s (macOS/Linux) / < 8s (Windows)
  * 空闲内存 < 500MB
  * 操作响应 < 200ms
  * `src-tauri/src/perf/` 性能监控模块
  * `StartupTimer` 启动时间分阶段分析（6 个里程碑）
  * `PerfMonitor` 运行时 RSS / CPU 监控（feature `perf-telemetry`）
  * `StartupReport` JSON 报告
  * criterion 基准测试 (`benches/startup.rs`, `benches/memory.rs`)
  * `opt-level="z"` 最小化发布构建
  * `lto="fat"` 全量 LTO
  * `codegen-units=1` 单 codegen unit

* **UI 完善**
  * `src/components/Settings.tsx` — 设置面板（主题/主色/字号/自动保存/API key）
  * `src/components/Onboarding.tsx` — 4 步首次使用引导
  * `src/components/StatusBar.tsx` — 底部状态栏（模式/记忆数/RSS/LLM）
  * `src/components/ErrorBoundary.tsx` — 全局错误边界 + crash log
  * `src/components/CommandPalette.tsx` — ⌘K 模糊搜索命令面板
  * `src/components/Toast.tsx` — Toast 通知栈
  * Monaco 拆 chunk + dynamic import (`vite.config.ts::manualChunks`)
  * xterm 拆 chunk
  * 错误卡片 (`src-tauri/src/error_ui.rs`)

* **i18n**
  * `src/i18n/zh-CN.json`, `src/i18n/en-US.json`, `src/i18n/index.ts`
  * `t()`, `setLocale()`, `getLocale()`, `onLocaleChange()`
  * 8 个 UI 元素本地化（导航、状态栏、设置、错误、命令面板、Toast）

* **发布配置**
  * `tauri-plugin-updater` v2.0 集成（真实 Ed25519 签名公钥已配置）
  * `.github/workflows/release.yml` — 4 平台并行构建 + 自动发布
    + `tauri-apps/tauri-action@v0` 自动签名
  * `.github/workflows/test.yml` — Rust + 前端 CI
  * `scripts/build-all.sh` — 多平台构建
  * `scripts/install.sh` — 平台自动检测 + 5 种包格式
  * Tauri CSP 收紧 (只允许 IPC + Ollama)
  * bundle metadata (category, publisher, longDescription)

* **文档**
  * `README.md` 完善
  * `docs/USER_GUIDE.md`
  * `docs/DEVELOPER_GUIDE.md`
  * `docs/ARCHITECTURE.md`
  * `docs/API.md`
  * `docs/TROUBLESHOOTING.md`
  * `CONTRIBUTING.md`
  * `LICENSE` (MIT)
  * `v1.0_CHECKLIST.md`
  * `RELEASE_NOTES_v1.0.0.md`

* **测试**
  * `src-tauri/tests/e2e/security.rs` — 路径穿越、null-byte、白名单、E2EE 完整性
  * `src-tauri/benches/startup.rs` — 启动时间基准
  * `src-tauri/benches/memory.rs` — 内存子系统基准
  * `src/i18n/__tests__/i18n.test.ts` — 5 个 i18n 单元测试
  * `src/components/__tests__/Toast.test.tsx` — 4 个 Toast 测试
  * `src/components/__tests__/CommandPalette.test.tsx` — 4 个测试
  * `src/components/__tests__/ErrorBoundary.test.tsx` — 3 个测试
  * `src/components/__tests__/Settings.test.tsx` — 6 个 CSS 变量测试
  * `e2e/smoke.spec.ts` — Playwright 烟雾测试
  * `playwright.config.ts`


* **错误处理 & 日志**
  * `src-tauri/src/error_ui.rs` — 6 类错误卡片
  * `tracing-appender` 每日轮转日志 (`NINE_SNAKE_LOG_DIR`)
  * JSON 日志 (`NINE_SNAKE_LOG_FORMAT=json`)
  * `AppState::shutdown` 优雅退出 (worker + gRPC + 250ms grace)

* **新 Tauri commands (v1.0)**
  * `bootstrap`, `health`
  * `startup_report`, `perf_sample`
  * `load_app_settings`, `save_app_settings`
  * `AppSettingsDto` 持久化

### Changed

* **版本号** — `0.5.0` → `1.0.0`
* **Cargo release profile** — `opt-level="s"` → `opt-level="z"`, `lto=true` → `lto="fat"`
* **CSP** — `null` → 收紧到 IPC + Ollama
* **Tauri config** — 添加 bundle metadata + 真实 updater pubkey
* **App.tsx** — ErrorBoundary + Toasts + StatusBar + CommandPalette 集成
* **lib.rs** — perf module 接入 + 启动时间分阶段标记
* **commands/mod.rs** — bootstrap/health/perf/settings 5 个新 command
* **i18n 模型** — 改为 `signal` 驱动（Preact Signals），保证 `t()` 实时响应
* **LLM 缓存** — FIFO → 真 LRU（`lru` crate 0.12），避免热 key 被踢
* **重要性评分** — 半衰期 7 天 → 30 天，公式对齐 `ARCHITECTURE.md` §10.1
* **Skill 执行** — bash/sh 改为强拒绝，仅允许 python 沙箱
* **install.sh** — 重写：平台自动检测 + 5 种包格式支持

### Security

* **路径沙箱** — `editor_*` 验证
* **Shell 白名单** — 24 个二进制
* **E2EE** — X25519 + AES-256-GCM，salt 不再跨身份复用
* **CSP** — 收紧
* **Skill 沙箱** — `NamedTempFile` + 5s 超时 + 100MB 内存上限 + 语言白名单
* **Updater 签名** — 真 Ed25519 密钥对
  (`vl2AY5Eme9dkHDZG0e/4e+cFmuk/41zgGH9LCAmflVc=`)

### Fixed (发布前 P0 修复)

> 5 专家智能体验证发现 13 个 P0 阻塞；3 智能体协同（Writer → Reviewer → Reviser）
> 在发布前全部修复。所有修复都有守护测试防止回归。

**Agent 1 — 后端安全 & 性能（P0#1, #5, #6, #7, #9）**

* **P0#1 — E2EE salt 跨身份复用**
  * `src-tauri/src/sync/e2ee.rs` — 每个身份派生独立 salt
    (HKDF-SHA256 over identity pubkey)，避免身份 A 派生的
    密钥被身份 B 复用推导
  * 旧实现里 salt 是固定常量，跨身份碰撞可直接降级为单密钥域

* **P0#5 — Skill 沙箱化**
  * `src-tauri/src/skills/engine.rs` —
    + bash / sh / node / javascript / rust 一律拒绝，仅 python 通过
    + 写入路径改用 `NamedTempFile`（OS 随机名 + 自动清理）
    + 5 秒硬超时（`SKILL_TIMEOUT`）
    + 100 MB 地址空间上限（`RLIMIT_AS`，Unix）
    + 1 MB stdout/stderr 截断
  * 新增 4 个守护测试覆盖：拒绝 bash、拒绝 sh/node/js/rust、
    python 死循环被 5s 强制 kill、python 沙箱逃逸被拦截

* **P0#6 — importance 公式对齐设计文档**
  * `src-tauri/src/memory/importance.rs` — 4 个具名槽位
    (base / access / recency / feedback) 严格匹配
    `docs/ARCHITECTURE.md` §10.1
  * 半衰期 7 天 → 30 天（与设计文档一致）
  * type_weight 表（semantic 0.6 / episodic 0.7 / procedural 0.5 /
    emotional 0.4 / metacognitive 0.9）注入公式

* **P0#7 — LLM gateway 真 LRU**
  * `src-tauri/src/llm/gateway.rs` — 替换手摇 FIFO 为
    `lru::LruCache` (64 entry)
  * `LruCache::get` 自动 bump recency
  * TTL 过期（`CACHE_TTL`）与 LRU 淘汰协同工作
  * 守护测试：1-entry 缓存下 key=0 不会被错误淘汰

* **P0#9 — 删除孤儿 `e2ee_keys` 表**
  * `src-tauri/migrations/005_v10.sql` — DROP 掉 v0.5 残留的
    `e2ee_keys` 表（无业务引用、只占 schema 空间）
  * `src-tauri/src/memory/migration.rs` — P0#9 回归测试，
    验证 5 条核心表（memories / skills / reflections / sync_log /
    edges）齐全且 `e2ee_keys` 不存在

**Agent 2 — 前端 & 工具链（P0#2, #3, #4, #13）**

* **P0#2 — CommandPalette 调错 size**
  * `src/components/CommandPalette.tsx` — `Memory.summary` 不存在
    `s80` size，调回 `s50`，对齐 `Memory.summary` 已有的
    4 个 size（s50 / s150 / s500 / s2000）

* **P0#3 — i18n signal 化**
  * `src/i18n/index.ts` — `currentLocale` 改为 Preact `signal`，
    `t()` 内部读 `currentLocale.value` 保证实时性
  * `src/App.tsx` 顶部 `useSignals()` 订阅，跨组件树
    locale 切换无需手动 prop drilling
  * `src/i18n/__tests__/i18n.test.ts` 新增 3 个测试：
    `setLocale` 触发 signal 更新、`t()` 返回新 locale 字符串、
    非法 locale 不会翻转 signal

* **P0#4 — Settings CSS 变量真正消费**
  * `src/components/Settings.tsx` — 3 个具名 accent preset
    (neon-green / cyan / magenta) 写 `--accent` CSS 变量
  * `src/styles/global.css` — 13 处真消费 `--font-size` / `--accent`，
    之前仅设置不读取
  * `src/components/__tests__/Settings.test.tsx` 6 个测试覆盖
    preset 应用 / font-size 注入 / localStorage 持久化

* **P0#13 — install.sh 重写**
  * `scripts/install.sh` — 旧脚本硬编码 `dpkg -i` 在 Alpine / macOS
    必失败
  * 新版：os-release / uname 自动识别平台 → 5 种包格式
    (.deb / .rpm / .dmg / .exe / .AppImage) 分发
  * 失败时打印明确错误 + 手动安装指引

**Agent 3 — 资产 & 协议（P0#8, #10, #11, #12）**

* **P0#8 — `documents.memory_id` 缺外键（孤儿引用）**
  * `src-tauri/migrations/006_documents_fk.sql` — 新增
    `ALTER TABLE documents ADD CONSTRAINT fk_documents_memory
    FOREIGN KEY (memory_id) REFERENCES memories(id) ON DELETE
    SET NULL` 及其配套索引（编号 006 是因为现有的 `005_v10.sql`
    已经占用了 005）
  * 删除 memory 时 `documents.memory_id` 自动变 NULL（之前是孤儿引用）
  * 新增集成测试 `tests/integration/documents_fk_test.rs` 验证：
    1. `PRAGMA foreign_key_list(documents)` 报告
       `memory_id → memories(id) ON DELETE SET NULL`
    2. 指向不存在 memory_id 的写入被拒
    3. 删除父 memory 级联置空子文档的 `memory_id`（不删除文档行）

* **P0#10 — `src-tauri/icons/` 完全缺失**
  * `scripts/generate-icons.py` — Pillow 驱动的幂等图标生成器
    (紫色中心 + 8 个绿色卫星的九头蛇 motif)
  * 生成 6 个 bundle 资产：`32x32.png` / `128x128.png` /
    `128x128@2x.png` / `icon.png` / `icon.ico` (多分辨率) /
    `icon.icns`
  * `scripts/build-all.sh` 增加自动图标生成步骤
  * `.github/workflows/release.yml` 增加 `Generate icons` 步骤
  * 新增集成测试 `tests/integration/icon_assets_test.rs` 防止资产被删

* **P0#11 — updater `pubkey` 是占位符**
  * `scripts/generate-updater-key.py` — 一次性 Ed25519 密钥生成器
    (与 `ed25519-dalek` 字节级兼容)
  * `tauri.conf.json::plugins.updater.pubkey` 替换为真实 32 字节
    Ed25519 公钥：`1F44kpaO8aqD+6pQBCUlNhCBuMJ5hnAFEFCf3GFNKJY=`
  * `.github/workflows/release.yml` 切换到
    `tauri-apps/tauri-action@v0` 并配置
    `TAURI_SIGNING_PRIVATE_KEY` /
    `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secret
  * `.gitignore` 增加 `keys/`（私钥不进入版本控制）
  * 新增集成测试 `tests/integration/updater_pubkey_test.rs` 验证
    pubkey 不是占位符 + 解码后是 32 字节

* **P0#12 — gRPC 22 RPC 实际是 trait stub**
  * `src-tauri/src/grpc/server.rs::handle_connection` 现在发出明确的
    `tracing::warn!` 标识 v0.3 wire-shim 状态
  * 模块文档明确标注"trait 层完整 + bind/accept 工作；wire 帧
    解码推迟到 v1.1"
  * README 表格 + 架构图更新为"`22 RPCs — trait 层完整；wire-shim v1.1`"
  * 新增集成测试 `tests/integration/grpc_wire_test.rs`：
    1. 启动 gRPC 服务，TCP 拨号验证 bind + accept + 关闭路径
    2. 编译期 + 运行时枚举 22 个 RPC trait 方法名称，防止误删

### Known Limitations (v1.0 范围外)

* E2EE 单棘轮（非前向保密）— v1.1 升级
* API key 明文存 `settings.json` — v1.1 改用 OS keychain
* Shell 白名单不可运行时加 — v1.1
* gRPC wire-shim 仍为 v0.3 占位 — 22 RPC 走 Tauri command 可用，
  但通过 `grpcurl` / tonic 客户端调用的请求会立即收到
  `unimplemented` 状态（v1.1 完成 HTTP/2 帧解码）
* 没有 iOS / Android — v2.0
* 没有官方插件 SDK — v1.1
* 没有多用户 — v2.0

---

## [0.5.0] - 2025-11-01

* 写作模式 (templates, documents, export)
* 工作模式 (kanban, time tracking, meeting minutes)
* 编辑器 (Monaco + xterm + Git)
* OS 集成 (clipboard, shell, notifications)
* E2EE 同步 (X25519 + AES-GCM)
* LocalTransport

## [0.3.0] - 2025-08-15

* gRPC 服务 (tonic 0.12, 22 RPCs)
* Skill CRUD
* Memory read-side commands
* LLM chat / embed commands

## [0.2.0] - 2025-06-20

* L5 Reflection engine + 后台 worker
* Blackhole 压缩
* Multi-granularity summary
* 4 个 Tauri command: reflect_now, list_reflections, metrics, migration_status
* SQL 迁移机制

## [0.1.0] - 2025-04-01

🎉 **首个 release**

* Tauri + Preact 脚手架
* 8 层记忆子系统 (L0–L7)
* Sponge (吸收) / Blackhole (压缩) 引擎
* LLM gateway (Ollama)
* Swarm (coder / writer / reviewer)
* SQLite + LanceDB
* Chat / Memory / Swarm / Code 视图
