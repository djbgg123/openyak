# openyak Rust 工作区

`rust/` 是当前仓库的主产品实现面。这里包含可直接构建、运行和维护的 `openyak` CLI，以及其运行时、工具系统、插件框架和配套服务。

最近一次全量文档与命令面对齐完成于 `2026-04-09`。本文内容已对照当前 `openyak` CLI help、最新一轮 59 项 release-binary 逐命令 rerun，以及工作区、根目录 Python、Python SDK、TypeScript SDK 本地验证结果更新。

## 当前定位

openyak 受 Claude Code 启发，但采用清洁重写实现。当前 Rust 工作区已经不是实验性骨架，而是一套可直接用于本地 coding-agent 工作流的主实现。

当前对外能力边界需要明确：

- Task / Team / Cron 当前提供的是 registry-backed、operator-facing foundation slices，不是完整的持久化服务。
- LSP / MCP 当前重点是 registry-backed bridge 和可观测能力，不是完整的 user-facing control plane。
- `openyak server` 已通过 `openyak server` 命令作为本地 HTTP/SSE thread server surfaced，暴露 `/v1/threads` 与 legacy `/sessions` compatibility routes，把线程状态持久化到工作区 `.openyak/state.sqlite3`，并将 bind 范围限制在 loopback 地址。
- `openyak server` 不是 codex-style full app-server，也不是远程控制面。
- 当前 `/v1/threads` 已具备最小持久化与 restart-recovery 原语：如果 server 在 run 中途重启，线程快照会恢复成 `interrupted` 状态并附带 `recovery_note`；当前 thread contract 已显式标注 `truth_layer = daemon_local_v1`、`attach_api = /v1/threads`，但这仍只覆盖 thread attach-first truth，不代表 Task / Team / Cron 已升级成 daemon-backed orchestration layer。
- Task / Team / Cron runtime/tool payload 现在也会显式回传结构化 `contract` 元数据（`truth_layer = process_local_v1`、`operator_plane = local_runtime_foundation_v1`、`persistence = process_memory_only_v1`），用来把 foundation lifecycle/failure 叙事继续锁在 process-local 边界，而不是借 thread daemon truth 过度类比。
- 同一工作区下如果存在重叠生命周期的多个 `openyak server` 实例，thread discovery 文件现在会按 `pid` 做 owner-safe 清理，较早退出的实例不会误删较新实例的发现入口。
- thread/session 工具现在会同时尝试当前工作区路径和 canonical 工作区路径下的 thread discovery 文件；通过 symlink、junction 或其他等价路径进入同一工作区时，仍能发现正在运行的本地 `openyak server`。
- `sdk/typescript` 与 `sdk/python` 已提供 attach-first、本地-only 的 alpha SDK，公共边界锁定在当前 `/v1/threads` 合约，不包含 launcher/runtime bundling，也不把 `/sessions` 暴露为 SDK 公共边界。
- `openyak doctor` 已提供本地只读的 config/auth/runtime 健康检查入口。
- `openyak doctor` 现在会尊重全局 `--model`；当你使用 OpenAI-compatible gateway/API key 时，可以用 `openyak --model <openai-family-model> doctor` 预检与 prompt / REPL / GitHub workflow 相同的 auth path。
- `openyak foundations [task|team|cron|lsp|mcp]` 与 `openyak /foundations [task|team|cron|lsp|mcp]` 已提供只读的 operator discovery surface，用于解释当前 Task / Team / Cron / LSP / MCP 五族的 tool membership 与边界。
- 本地 `toolProfiles` 配置已可通过 `openyak --tool-profile <name> ...` 作用到当前 REPL 或单次 prompt，用来收窄 permission mode / allowed-tools ceiling；该能力仍然是 process-local、session transcript-only，且 sandbox 附加约束当前只覆盖 `bash`。
- 可选 hidden browser tools 现在已经接入：`BrowserObserve` 提供单次 rendered-page observe，`BrowserInteract` 提供单次 selector-backed click 后回传页面状态；两者都要求 `browserControl.enabled=true`、显式 `--allowedTools BrowserObserve` / `BrowserInteract` 和 `danger-full-access`，并继续保持 CLI-local、single-call、无持久 browser session、无 `/v1/threads` browser support。
- 顶层 help 路由、逐个顶层命令 help、直接命令、skills lifecycle、direct slash CLI、resume-safe slash command 链路，以及 `login` / `onboard` / `prompt` / self-target `package-release` 的受控失败路径现在已由二进制 smoke/regression 测试锁定，不再只依赖一次性的人工巡检。
- 插件 manifest 的文件路径现在会做边界校验；解析后的结果必须保持在插件根目录内。
- 插件配置路径现在也会做边界校验：`plugins.installRoot` / `plugins.registryPath` 必须留在用户级 config home 内，repo-scoped `plugins.externalDirectories` / `plugins.bundledRoot` 必须留在当前工作区内；managed plugin update/uninstall 也会拒绝越出 managed install root 的路径。
- `/commit` 现在会基于完整 workspace status 和 `git diff --stat HEAD` 生成 Lore commit 草稿，不再只看已暂存变更；`/pr` 现在会基于当前分支相对默认分支的 diff 生成标题和正文。
- `openyak server` 在 runtime/provider bootstrap 失败时，仍会把刚提交的 turn 或 user-input response 乐观写回线程状态，attach-first 客户端不会再因为这类早期失败而丢失用户输入痕迹。
- 内置 `REPL` tool 现在会真正执行 `timeout_ms`；超时后会终止子进程，返回 `exitCode=124`，并在 `stderr` 里附带明确的 timeout 文本，而不再静默忽略超时参数。

当前版本：

- 版本号：`0.1.0`
- 发布阶段：源码构建可用，打包分发仍在完善
- 主二进制：`openyak`

## 公开协作基线

公开仓库的根目录现在提供：

- [`../LICENSE`](../LICENSE)
- [`../SECURITY.md`](../SECURITY.md)
- [`../CODE_OF_CONDUCT.md`](../CODE_OF_CONDUCT.md)
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md)

GitHub Actions 基线位于 [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml)。它镜像当前仓库已经接受并可本地复现的验证命令：Rust 工作区、根目录 Python 对照层、Python SDK、TypeScript SDK。

这条 CI 基线只负责验证，不代表 release 上传、签名或自动分发已经完成。

## 前置要求

- Rust stable 工具链
- Cargo
- 你要使用的模型 Provider 凭据
- 如果要跑 GitHub PR/Issue 链路，需要本机可用的 `gh`

## 构建与运行

### 构建

```bash
cargo build --workspace
cargo build --release -p openyak-cli
```

### 运行 CLI

```bash
cargo run --bin openyak -- --help
cargo run --bin openyak -- --version
cargo run --bin openyak --
cargo run --bin openyak -- "总结这个工作区"
cargo run --bin openyak -- prompt "总结这个工作区"
cargo run --bin openyak -- --tool-profile audit prompt "只读审查这个工作区"
cargo run --bin openyak -- dump-manifests
cargo run --bin openyak -- bootstrap-plan
cargo run --bin openyak -- skills
cargo run --bin openyak -- skills available
cargo run --bin openyak -- agents
cargo run --bin openyak -- system-prompt --date 2030-02-03
cargo run --bin openyak -- onboard
cargo run --bin openyak -- doctor
cargo run --bin openyak -- foundations
cargo run --bin openyak -- server --help
cargo build --release -p openyak-cli
```

类 Unix release binary：

```bash
./target/release/openyak package-release --output-dir dist
```

Windows PowerShell release binary：

```powershell
.\target\release\openyak.exe package-release --output-dir dist
```

最近一轮 packaged-use 验收已确认：`openyak package-release` 会生成形如 `dist/openyak-0.1.0-<target>/` 的目录，其中至少包含 `openyak(.exe)`、`INSTALL.txt` 和 `release-metadata.json`。如果 `--binary` 已经指向目标输出目录里的现成 artifact，命令会显式拒绝 self-target packaging，而不是在 Windows 上退化成模糊的复制权限错误。

### 初始化目标项目

```bash
cargo run --bin openyak -- init
```

`openyak init` 当前会为目标项目脚手架生成：

- `OPENYAK.md`
- `.openyak.json`
- `.openyak/`
- 推荐的本地 `.gitignore` 条目

直接运行构建产物：

类 Unix：

```bash
./target/debug/openyak
./target/debug/openyak prompt "解释 crates/runtime"
```

Windows：

```powershell
.\target\debug\openyak.exe
.\target\debug\openyak.exe prompt "解释 crates/runtime"
```

## 认证与 Provider 配置

### API Key 模式

按所选 Provider 配置对应环境变量即可。如果使用兼容端点，通常还需要同时设置对应的 `*_BASE_URL`。

Anthropic 兼容示例：

```bash
export ANTHROPIC_API_KEY="..."
export ANTHROPIC_BASE_URL="https://api.anthropic.com"
```

PowerShell 写法：

```powershell
$env:ANTHROPIC_API_KEY = "..."
$env:ANTHROPIC_BASE_URL = "https://api.anthropic.com"
```

OpenAI 兼容示例：

```bash
export OPENAI_API_KEY="..."
export OPENAI_BASE_URL="https://api.openai.com/v1"
```

Grok 示例：

```bash
export XAI_API_KEY="..."
export XAI_BASE_URL="https://api.x.ai"
```

### OAuth 模式

```bash
cargo run --bin openyak -- login
cargo run --bin openyak -- logout
```

`openyak login` 不再内置默认 OAuth 站点。要使用它，必须先在 `settings.oauth` 里显式配置至少这三个字段；OAuth 后端由你自己提供，CLI 不会替你补任何默认 URL。仓库提供两份可直接填写的模板：

- loopback 回调版：[`docs/oauth.settings.loopback.template.json`](./docs/oauth.settings.loopback.template.json)
- 手动回调版：[`docs/oauth.settings.manual-redirect.template.json`](./docs/oauth.settings.manual-redirect.template.json)

- `clientId`
- `authorizeUrl`
- `tokenUrl`

可选字段：

- `callbackPort`：本地回调端口；未设置时默认使用 `4545`
- `manualRedirectUrl`：启用手动回调模式，不再监听本地 `localhost`
- `scopes`

示例：

```json
{
  "oauth": {
    "clientId": "your-client-id",
    "authorizeUrl": "https://auth.example.com/oauth/authorize",
    "tokenUrl": "https://auth.example.com/oauth/token",
    "manualRedirectUrl": "https://auth.example.com/oauth/callback",
    "scopes": ["openid", "profile"]
  }
}
```

行为说明：

- 未配置 `manualRedirectUrl` 时，`openyak login` 使用本地回调地址 `http://localhost:<port>/callback`
- 配置了 `manualRedirectUrl` 时，`openyak login` 会要求手动粘贴最终回跳 URL 或 query string
- OAuth token 优先写入系统凭据库；仅在系统凭据库不可用时，才回退到用户级配置目录下的 `credentials.json`
- 如果你要接自己的认证后端，把 `authorizeUrl` / `tokenUrl` / `manualRedirectUrl` 指向你的服务即可；未配置时 `openyak login` 会直接报错，而不是使用默认站点
- 如果你想使用本地 loopback 回调，可以保留 `callbackPort` 并删除 `manualRedirectUrl`

### GitHub CLI

如果要在交互式 `openyak` 会话里使用 `/pr`、`/issue`、`/commit-push-pr` 等链路，先完成：

```bash
gh auth login --web
```

Windows 下的 `gh` 解析和浏览器启动已经统一走运行时 helper；如果链路失败，优先检查相应命令是否真的在系统环境中可解析。

- 这些是 REPL slash command，不支持 `openyak /pr ...` 这种 direct slash CLI 入口；请先启动 `openyak`
- 这三条链路同时依赖 `gh auth status` 和活动模型鉴权，因为 openyak 会先生成草稿，再联系 GitHub
- `openyak doctor` 会把 GitHub CLI 可解析性、`gh auth status` 与活动模型本地 auth bootstrap 一起列出来，适合作为只读预检

`2026-04-08` 的真实验收已经证明：只要给出 OpenAI-compatible gateway/API key，并显式选择 OpenAI-family model，当前实现就可以成功跑通 provider-backed `doctor` / `prompt` / 单轮 REPL，以及 disposable GitHub `/issue`、`/pr`、`/commit-push-pr`。

```bash
export OPENAI_BASE_URL="https://your-openai-compatible-gateway/v1"
export OPENAI_API_KEY="..."
cargo run --bin openyak -- --model gpt-5.3-codex doctor
cargo run --bin openyak -- --model gpt-5.3-codex prompt "reply with the exact text: OPENYAK_PROMPT_OK"
printf 'reply with the exact text: OPENYAK_REPL_OK\n/exit\n' | cargo run --bin openyak -- --model gpt-5.3-codex
```

- `openyak login` 仍然是独立 OAuth 链路；API key 成功并不意味着 `openyak login` 可用，缺少 `settings.oauth.clientId` / `authorizeUrl` / `tokenUrl` 时它会继续拒绝执行。
- 推荐的 disposable GitHub 验收顺序是：临时私有仓库 `/issue` -> 手工 commit + push throwaway branch 后 `/pr` -> 回到 `main` 保留未提交改动后 `/commit-push-pr`。
- 清理时优先执行 `gh repo delete OWNER/REPO --yes`。如果当前 `gh` token 缺少 `delete_repo` scope，先用 `gh issue close` / `gh pr close -d` 清掉 issue、PR 和远端分支，再运行 `gh auth refresh -h github.com -s delete_repo` 补齐删除权限后重试仓库删除。

如果要先做本地健康预检：

```bash
cargo run --bin openyak -- doctor
```

如果想要一个显式、可重跑的本地 onboarding flow，把 repo init、默认模型、auth guidance 和 doctor handoff 串起来：

```bash
cargo run --bin openyak -- onboard
```

`openyak onboard` 只在交互式本地终端中运行；它不会改变 `openyak` 无参数默认进入 REPL 的语义，也不会把 provider secrets 写进配置文件。在非交互终端中调用时，它会直接拒绝执行并说明原因。

## 用户目录、配置与 skills 规则

当前工作区已经统一了用户目录和配置目录解析，优先级如下：

1. `OPENYAK_CONFIG_HOME`：显式指定用户级 `.openyak` 目录
2. `CODEX_HOME`：显式指定用户级 `.codex` 目录，并用其父目录推导默认用户根目录
3. 平台默认用户目录

平台默认用户目录的回退顺序：

- Windows：`USERPROFILE`，其次 `HOMEDRIVE` + `HOMEPATH`，最后 `HOME`
- macOS / Linux：`HOME`，其次 `USERPROFILE`，再其次 `HOMEDRIVE` + `HOMEPATH`

如果这些环境变量都不可用，运行时最后回退到系统临时目录，而不是当前工作目录。

这套规则覆盖：

- OAuth 凭据读写
- 全局 `settings.json`
- `openyak agents`
- `openyak skills`
- `Skill` 工具
- 远程相关默认路径

项目级配置仍然使用：

- `.openyak.json`
- `.openyak/settings.json`
- `.openyak/settings.local.json`

skills 目录支持两种布局：

- `skills/<name>/SKILL.md`
- `skills/.system/<name>/SKILL.md`

`openyak skills` 和 `Skill` 工具使用同一套发现/解析逻辑，因此它们看到的结果一致。

插件路径规则：

- `settings.plugins.installRoot` 和 `settings.plugins.registryPath` 的相对路径按用户级 config home 解析，最终路径也必须保持在该目录内。
- user config 中的 `settings.plugins.externalDirectories` / `settings.plugins.bundledRoot` 非绝对路径默认相对 config home；如果显式写成 `./...`，则按当前 workspace 解析。
- project/local config 中的 `settings.plugins.externalDirectories` / `settings.plugins.bundledRoot` 一律按当前 workspace 解析，且不能通过 `..` 或其他方式逃逸出当前仓库。

系统提示和状态输出中的当前日期由运行时在执行时生成，不再依赖仓库中的硬编码日期常量。

## 当前命令面

### 顶层命令

- `openyak`
- `openyak prompt "..."`
- `openyak agents`
- `openyak skills [list|available|info <skill-id>|install <skill-id>|update <skill-id>|uninstall <skill-id>]`
- `openyak login`
- `openyak logout`
- `openyak init`
- `openyak onboard`
- `openyak doctor`
- `openyak foundations [task|team|cron|lsp|mcp]`
- `openyak package-release [--output-dir PATH] [--binary PATH]`
- `openyak server [--bind HOST:PORT]`
- `openyak server start --detach [--bind HOST:PORT]`
- `openyak server status`
- `openyak server stop`
- `openyak dump-manifests`
- `openyak bootstrap-plan`
- `openyak system-prompt`

### 代表性 slash command

- 状态与会话：`/status`、`/compact`、`/clear`、`/session`、`/resume`、`/export`
- 配置与内存：`/config`、`/memory`、`/foundations`、`/init`、`/diff`、`/version`
- 交互控制：`/model`、`/permissions`
- 发现与诊断：`/agents`、`/skills`、`/teleport`、`/bughunter`、`/ultraplan`、`/debug-tool-call`
- Git/GitHub：`/branch`、`/worktree`、`/commit`、`/pr`、`/issue`、`/commit-push-pr`
- 插件：`/plugin`、`/plugins`

## 当前能力面

当前 Rust 主实现已经支持：

- 交互式 REPL 与单次 prompt 执行
- 会话保存、查看、恢复、导出
- 内置工具：shell、文件读写/编辑、搜索、Web fetch/search、todo、notebook、skill、agent、tool search 等
- 可选的本地 browser tools：`BrowserObserve` 用于单次 rendered-page observe，`BrowserInteract` 用于单次 selector-backed click 后回传页面状态；两者都要求 `browserControl.enabled=true`、显式 `--allowedTools BrowserObserve` / `BrowserInteract` 和 `danger-full-access`，并继续保持 CLI-local、single-call、无持久 browser session、无 `/v1/threads` browser support
- attach-first、本地-only 的 TypeScript 与 Python SDK alpha，直连 `openyak server` 当前 `/v1/threads` 协议
- local-only 的 Session operator surface：`SessionList`、`SessionGet`、`SessionCreate`、`SessionSend`、`SessionResume`、`SessionWait`
- 顶层命令与 REPL slash command 的本地发现和执行
- direct slash CLI 入口（`openyak /agents`、`openyak /skills`、`openyak /foundations`）以及 `--resume` 形式的 resume-safe slash command 恢复执行
- `openyak doctor` 对配置加载、OAuth 配置/凭据、活动模型鉴权预检，以及 GitHub CLI 可用性 / `gh auth status` readiness 做本地只读检查
- `openyak foundations [task|team|cron|lsp|mcp]` / `/foundations [task|team|cron|lsp|mcp]` 作为只读的 discovery surface，明确说明当前 Task / Team / Cron / LSP / MCP 的 tool membership、truth label 与 `process_local_v1` / registry-backed 边界
- `openyak package-release` 生成本地 release artifact 目录，供 release/upload 与脱离源码目录的 packaged-use 验证
- `openyak server start --detach` 作为第一个 local-only operator start action：只会在 detached launch 前做 workspace discovery preflight，并在确认 running discovery record 可用后返回
- `openyak server status` 作为首个最小 CLI-first daemon/operator inspection：只读回显当前工作区 thread server 的 discovery、reachability、`daemon_local_v1` contract labels 与本地状态库存在性
- `openyak server stop` 作为第一个 local-only operator action：只会在验证 reachable operator identity 与当前工作区 discovery/pid 一致后停止本地 thread server，并在 stale registration 场景下清理旧发现记录
- 插件发现、安装、启用、禁用、卸载、更新
- 插件工具聚合、插件 hook 聚合与生命周期
- mock parity harness 基础设施，以及一批 registry-backed parity foundation tools（Task/Team/Cron + LSP/MCP registry surface）
- MCP stdio、OAuth、registry-backed LSP/MCP operator bridges，以及 `openyak server` 暴露的本地 HTTP/SSE thread/session surface
- Git / GitHub 工作流命令
- 顶层子命令专属 `--help` 输出，以及 `/diff` 对未跟踪文件的正确展示

## Registry-backed parity foundation

当前 `rust/` 已经有一批低风险、registry-backed 的 parity foundation slices，可直接作为 operator-facing baseline 理解：

- Task lifecycle：`TaskCreate` / `TaskGet` / `TaskList` / `TaskStop` / `TaskUpdate` / `TaskOutput` / `TaskWait`
- Team / Cron foundation：`TeamCreate` / `TeamGet` / `TeamList` / `TeamDelete` / `CronCreate` / `CronGet` / `CronDisable` / `CronEnable` / `CronDelete` / `CronList`
- Session operator surface：`SessionList` / `SessionGet` / `SessionCreate` / `SessionSend` / `SessionResume` / `SessionWait`
- LSP registry query bridge：`LSP`（含 registry server listing / status / diagnostics）
- MCP registry bridge：`ListMcpServers` / `ListMcpTools` / `ListMcpResources` / `ReadMcpResource` / `McpAuth` / `MCP`

当前也已经有一个明确的只读发现入口：

- `openyak foundations [task|team|cron|lsp|mcp]`
- `openyak /foundations [task|team|cron|lsp|mcp]`

它们用于解释这五族当前已经发货的 operator surface，而不是把它们包装成更宽的 control plane。

其中 `Session*` 是 OP6 phase-1 的 hybrid local-only surface：thread-kind mutation 通过当前本地 `openyak server` 的 `/v1/threads` 真值面完成，`managed_session` 保持只读，`agent_run` 保持只读/有限 wait；其余能力继续建立在 `runtime` 里的 in-memory registries / bridges 之上。

其中 thread-kind `SessionList` / `SessionGet` / `SessionCreate` / `SessionWait` 现在会直接回显 daemon-backed contract metadata（`truth_layer` / `operator_plane` / `persistence` / `attach_api`）与恢复 guidance（`recovery_note`、`recovery.failure_kind` / `recovery.recovery_kind` / `recovery.recommended_actions`），避免把 daemon-backed thread truth 和 `process_local_v1` foundations 混成同一层 operator 叙事。

把这组能力放到 daemon/control-plane roadmap 上理解时，当前边界应视为：

- 已有：thread 级 durable snapshot、`truth_layer = daemon_local_v1` 的 thread contract、`operator_plane = local_loopback_operator_v1` / `persistence = workspace_sqlite_v1` contract labels、restart 后的 `interrupted` + `recovery_note`，以及 `failure_kind` / `recovery_kind` / `recommended_actions` 组成的结构化恢复 guidance。
- 这批恢复字段当前只覆盖 attach-first thread truth，可视为已发货的最小 `failure taxonomy / recovery recipes` slice，而不是更宽的 daemon control plane。
- 已有 operator-facing truth labels：thread snapshot 显式声明 `truth_layer = daemon_local_v1` 与 `attach_api = /v1/threads`；Task / Team / Cron registry payload 则继续声明 `origin = process_local_v1`。
- 已有 shared lifecycle/failure/recovery schema family：thread truth 公开 `contract` / `state` / `recovery` 三层快照；Task / Team / Cron 则只在 `process_local_v1` 边界内复用 lifecycle metadata（`created_at`、`updated_at`、`last_error`、`disabled_reason`、`capabilities`），没有被升级成 daemon-backed recovery plane。
- `/v1/threads/{id}/events` 的 `run.*` SSE payload 现在也会 additive 地回传 `status + lifecycle` 元数据，用于把运行中/完成/等待输入/失败语义锁进同一套 shared schema，而不是引入新的 daemon service lifecycle plane。
- `run.failed` 的 runtime/storage 两类失败路径现在也有 fixture + live regression 锁定，统一落到 shared `failure_kind / recovery_kind / recommended_actions` taxonomy，而不是继续漂移成 event-local 错误形状。
- 已有：`openyak server start --detach` 作为 local-only 的最小 CLI-first detached start surface。
- 已有：`openyak server status` 作为只读、local-only 的最小 CLI-first daemon operator inspection。
- 已有：`openyak server stop` 作为第一个最小 local-only operator action，但仍然只覆盖当前工作区 thread server。
- 未有：daemon-backed worker/task/team truth layer、跨 family 的 daemon lifecycle store、CLI-first daemon operator controls beyond local thread server start/status/stop。

当前 V1 contract 已冻结的核心口径：

- Task / Team / Cron registry 保持 `process_local_v1` 语义，只存在于当前 runtime 进程
- 当前不承诺持久化、恢复、租约 ownership、跨实例共享或 crash recovery
- 对外更重视稳定 metadata：`created_at`、`updated_at`、`last_error`、`disabled_reason`、`origin`、`capabilities`

更细的代码评审结论、能力矩阵和 staged follow-ups 见：[`docs/parity-foundation-registries.md`](./docs/parity-foundation-registries.md)。

## 工作区 crate 结构

- `openyak-cli`：主二进制、REPL、输出渲染、初始化与命令分发
- `api`：Provider 客户端与流式响应处理
- `runtime`：会话、配置、权限、prompt、OAuth、MCP 与运行时核心
- `tools`：内置工具实现
- `commands`：slash command 注册表与信息展示
- `plugins`：插件发现、注册表、hook 与生命周期
- `lsp`：语言服务器相关类型与 prompt 上下文增强基础
- `server`：本地 HTTP/SSE 服务端 crate（由 `openyak server` 暴露为本地 thread/session server）
- `compat-harness`：兼容性和迁移辅助

## 验证

最近一次工作区全量验收（`2026-04-09`）已覆盖：

```bash
cargo fmt --all --check
cargo build --workspace
cargo build --release -p openyak-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p openyak-cli --test command_surface_cli_smoke
cargo test -p openyak-cli --test doctor_cli_smoke
cargo test -p openyak-cli --test onboard_cli_smoke
cargo test -p openyak-cli --test package_release_cli_smoke
cargo test -p openyak-cli --test server_cli_smoke
cargo test -p openyak-cli --test mock_parity_harness
```

根目录 Python 对照层最近一次验收已覆盖：

```bash
python -m unittest discover -s tests -v
```

SDK 验证命令：

```bash
cd sdk/python
python -m pytest
python -m ruff check .
python -m mypy
python -m build
```

```bash
cd sdk/typescript
pnpm test
pnpm lint
pnpm build
```

上述验证命令现已同步固化到 GitHub Actions [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)，用于检查公开仓库可复现的主验证基线。

另外还做过一轮在 `2026-04-09` 完成的 fresh release binary 级命令面巡检，覆盖：

- `openyak --help`、`openyak --version`
- `openyak prompt`、`openyak dump-manifests`、`openyak bootstrap-plan`、`openyak agents`、`openyak skills`、`openyak foundations`、`openyak system-prompt`、`openyak login`、`openyak logout`、`openyak init`、`openyak onboard`、`openyak doctor`、`openyak package-release`、`openyak server` 的 help
- `openyak dump-manifests`、`openyak bootstrap-plan`、`openyak agents`、`openyak skills`、`openyak foundations`、`openyak system-prompt`、`openyak logout`、`openyak init`、`openyak doctor`、`openyak package-release` 的直接执行路径
- `openyak skills list/available/info/install/update/uninstall`
- `openyak /agents`、`openyak /skills`、`openyak /foundations`、`openyak /skills help`，以及 `openyak --resume ...` 的 resume-safe slash command 链路
- `openyak server --bind 127.0.0.1:0` 与 `openyak server start --detach --bind 127.0.0.1:0` 的真实启动探测
- `openyak server --bind 0.0.0.0:0` 的非 loopback bind 拒绝路径
- `openyak login`、`openyak onboard`、`openyak prompt` 与 self-target `openyak package-release` 的受控失败路径

其中 `openyak foundations lsp` 已做额外直接复核：当前 detail 输出使用高层 `Tools            LSP` 标签，不再展开成单条 `LspGetDiagnostics` 文案。

依赖外部环境的链路仍然需要单独准备条件后再做全链路验收：

- 模型调用：需要 Provider 凭据
- OAuth：需要你配置自己的后端
- GitHub：需要本机可用并已登录的 `gh`

Mock harness 的运行说明见：[`MOCK_PARITY_HARNESS.md`](./MOCK_PARITY_HARNESS.md)。

## 当前限制

- 当前已经支持通过 `openyak package-release` 生成 release artifact 目录，但压缩、上传和自动发布流程仍未完成。
- GitHub Actions CI 当前已经覆盖仓库主验证基线；正式发布、上传和分发流程仍需继续完善。
- 某些 live-provider 集成测试默认不启用，因为它们依赖真实外部凭据和网络环境。
- `openyak doctor` 当前只做本地只读预检，不提供自动修复、迁移或远程探测。
- Task / Team / Cron registry 当前仍是进程内临时状态，不提供 durability / restore / lease 语义。
- `openyak server` 当前提供 bind 范围限制在 loopback 地址的本地 thread/session HTTP/SSE 路由，不是完整 app-server 或远程控制面。
- daemon/control-plane 路线仍未发货独立的 service install/recover surface；当前只有 local-only `openyak server start --detach` / `openyak server status` / `openyak server stop`。现在的恢复语义仍只覆盖 thread attach-first compatibility，不等同于完整 daemon lifecycle management。
- 当前补齐的 LSP/MCP 仍以 registry-backed tool/operator surface 为主；完整独立 LSP main entry 继续作为分阶段 follow-up。
- 在 `0.x` 阶段，命令面和交互细节仍可能继续演进。

## 相关文档

- 仓库总览：[`../README.md`](../README.md)
- 仓库维护约定：[`../OPENYAK.md`](../OPENYAK.md)
- 公开贡献指南：[`../CONTRIBUTING.md`](../CONTRIBUTING.md)
- 安全策略：[`../SECURITY.md`](../SECURITY.md)
- 行为准则：[`../CODE_OF_CONDUCT.md`](../CODE_OF_CONDUCT.md)
- 许可证：[`../LICENSE`](../LICENSE)
- Rust 工作区维护契约：[`OPENYAK.md`](./OPENYAK.md)
- 贡献指南：[`CONTRIBUTING.md`](./CONTRIBUTING.md)
- Foundation parity/operator surface：[`docs/parity-foundation-registries.md`](./docs/parity-foundation-registries.md)
- Mock parity harness：[`MOCK_PARITY_HARNESS.md`](./MOCK_PARITY_HARNESS.md)
- 发布说明草案：[`docs/releases/0.1.0.md`](./docs/releases/0.1.0.md)
