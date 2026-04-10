# openyak

> Rust-first 的本地 coding-agent CLI 清洁重写项目。当前仓库中，`rust/` 是唯一按产品标准持续维护的主实现；`src/` 与 `tests/` 只承担对照、审计和迁移辅助职责。

[![CI](https://github.com/djbgg123/openyak/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/djbgg123/openyak/actions/workflows/ci.yml)

[`快速开始`](#快速开始) · [`公开仓库基线`](#公开仓库基线) · [`Fresh Clone 最小复现`](#fresh-clone-最小复现) · [`当前状态`](#当前状态) · [`仓库结构`](#仓库结构) · [`Rust 工作区说明`](./rust/README.md) · [`0.1.0 发布说明`](./rust/docs/releases/0.1.0.md) · [`贡献指南`](./CONTRIBUTING.md) · [`安全策略`](./SECURITY.md) · [`行为准则`](./CODE_OF_CONDUCT.md) · [`许可证`](./LICENSE)

最近一次全量文档与命令面对齐完成于 `2026-04-10`。本文内容已对照当前 `openyak --help` / `openyak skills help` / `openyak foundations --help` / `openyak server --help`、`2026-04-10` 的 47 步 release-binary 直接命令/子命令矩阵与 CLI smoke/regression rerun 更新；仓库级全量验证基线仍以 Rust、根目录 Python、Python SDK、TypeScript SDK 四条本地链路为准。

## 一眼看懂

- 主产品实现面是 `rust/`，可直接构建、运行并打包 `openyak` CLI。
- 当前主线已接通 REPL、单次 prompt、skills/agents、`openyak doctor`、`openyak foundations`、`openyak onboard`、`openyak package-release` 和 `openyak server`。
- `openyak server` 是 local-only 的 thread/session HTTP/SSE server，当前公共协议边界锁定在 `/v1/threads`；它不是 hosted control plane，也不是 codex-style full app-server。
- daemon/control-plane roadmap 当前仍处于 local-first 演进阶段：现有 `/v1/threads` 服务已经把线程状态持久化到工作区 `.openyak/state.sqlite3`，并会在 server 重启后把中断中的线程恢复成带 `recovery_note` 的 `interrupted` 快照；当前 thread contract 已显式标注 `truth_layer = daemon_local_v1`、`attach_api = /v1/threads`。当前已经发货 `openyak server start --detach` / `status` / `stop` / `recover` 这组 local-only operator 命令，但它们仍只覆盖当前工作区 thread truth，不是更宽的 hosted / remote control plane。
- `sdk/python` 和 `sdk/typescript` 是 attach-first、本地-only 的 alpha SDK，直接连接当前 `/v1/threads` 协议。
- 最近一次 fresh release-binary 命令面巡检完成于 `2026-04-10`；本轮直接执行了 47 个真实 release-binary help/命令/子命令步骤，并再次覆盖 detached/foreground server 生命周期、打包后二进制 `--help`、direct slash CLI，以及环境依赖路径的受控失败。

## 30 秒开始

```bash
cd rust
cargo build --release -p openyak-cli
cargo run --bin openyak -- --help
cargo run --bin openyak -- doctor
cargo run --bin openyak -- server start --detach --bind 127.0.0.1:0
cargo run --bin openyak -- server status
```

如果你只想先看最重要的用户能力，优先从这几个入口开始：

- `openyak --help`：看当前顶层命令面
- `openyak doctor`：做本地 config/auth/runtime 预检
- `openyak foundations`：看当前 Task / Team / Cron / LSP / MCP foundation 边界
- `openyak --tool-profile <name> ...`：对单次 prompt 或 REPL 应用本地命名 tool profile ceiling
- `openyak server --help`：看本地 thread server 的边界和运行方式

## 公开仓库基线

当前公开仓库已经补齐最小公开协作面：

- 根目录提供 `LICENSE`、`SECURITY.md`、`CODE_OF_CONDUCT.md`、`CONTRIBUTING.md`
- GitHub Actions CI 固化当前已验证的本地检查链路：Rust、根目录 Python 对照层、Python SDK、TypeScript SDK
- CI 的目标是复现当前可支持的本地验证，不负责 release 上传、签名或自动分发

如果你准备对外协作，先看根目录贡献/安全/行为文档；如果你准备改主产品实现，再进入 `rust/` 工作区文档和约束。

## Fresh Clone 最小复现

下面这条路径是当前仓库从全新 clone 开始的最小可复现入口：

```bash
git clone https://github.com/djbgg123/openyak.git
cd openyak
```

Rust 主线：

```bash
cd rust
cargo build --workspace
cargo test --workspace
cd ..
```

根目录 Python 对照层：

```bash
python -m unittest discover -s tests -v
```

Python SDK：

```bash
python -m pip install -e "sdk/python[dev]"
cd sdk/python
python -m pytest
python -m ruff check .
python -m mypy
python -m build
cd ../..
```

TypeScript SDK：

```bash
cd sdk/typescript
pnpm install --frozen-lockfile
pnpm test
pnpm lint
pnpm build
cd ../..
```

这条最小路径覆盖当前公开仓库已经承诺可复现的本地构建/测试面，不包含 OAuth 成功链路、live provider 调用、GitHub 远端操作或 release 上传。

## 项目定位

这个仓库的目标不是保存一份历史归档副本，而是围绕代理式 harness、工作区工具、权限控制、插件扩展、会话管理和 Git/GitHub 工作流，持续维护一套可构建、可运行、可验证的实现。

当前建议的理解方式是：

- 看产品行为与用户能力，优先看 `rust/`
- 看历史结构对照与迁移辅助，再看 `src/` 与 `tests/`
- 看维护约束、贡献方式和版本叙事，分别看根目录文档、`rust/README.md`、`rust/CONTRIBUTING.md` 与发布说明

## 当前状态

当前主线已经完成一轮面向真实使用的收口：

- Rust 工作区可直接构建并运行 `openyak` CLI。
- Rust 主线已经支持通过 `openyak package-release` 生成正式的本地 release artifact 目录。
- 交互式 REPL、单次 prompt、会话恢复、配置加载、插件管理、skills/agents 枚举都已接通。
- MCP、OAuth 与 Git/GitHub 工作流已经具备主实现；LSP/MCP 当前以 registry-backed operator surface 为主。
- `openyak server` 已作为本地 HTTP/SSE thread server 对外 surfaced，暴露 `/v1/threads` 与 legacy `/sessions` compatibility routes，把线程状态持久化到工作区 `.openyak/state.sqlite3`，并将 bind 范围限制在 loopback 地址。
- 当前 thread persistence / restart-recovery 只覆盖 thread 级 attach-first 状态：当 server 在 run 中途重启时，线程会恢复为 `interrupted` 并带 `recovery_note`，同时通过结构化 `recovery.failure_kind` / `recovery.recovery_kind` / `recovery.recommended_actions` 暴露恢复 guidance；这为后续 daemon/control-plane 路线提供恢复原语，但还没有把 Task / Team / Cron 等 foundation slices 提升成 daemon-backed orchestration truth。
- 面向 operator 的真值标签当前必须分开理解：thread snapshot 使用 `truth_layer = daemon_local_v1` 且 `attach_api = /v1/threads`；Task / Team / Cron foundation 仍明确保留 `origin = process_local_v1`，只表示当前 runtime 进程内的临时 registry metadata。
- thread/session operator surface 现在还会回显 `operator_plane = local_loopback_operator_v1` 与 `persistence = workspace_sqlite_v1`；这组 contract/recovery schema 只描述 `/v1/threads` attach-first truth，不代表 foundation registries 获得 daemon lifecycle store。
- 同一工作区存在重叠生命周期的多个 `openyak server` 实例时，本地 thread discovery 文件会按写入 `pid` 做 owner-safe 清理；较早退出的实例不会再误删较新实例的发现入口。
- thread/session 工具现在会同时尝试当前工作区路径和 canonical 工作区路径下的 thread discovery 文件；通过 symlink、junction 或其他等价路径进入同一工作区时，仍能发现正在运行的本地 `openyak server`。
- 已新增 attach-first、本地-only 的 SDK alpha：
  - `sdk/typescript`：直连当前主线 `openyak server` 的 `/v1/threads` 协议
  - `sdk/python`：在同一锁定协议上提供 sync/async parity alpha
- 两个 SDK 都不会自动拉起 server，也不会把 legacy `/sessions` 暴露为 SDK 公共边界。
- 用户目录、配置目录、skills 扫描、运行时日期来源已经统一，不再依赖分散的 `HOME` 逻辑或硬编码日期。
- `openyak login` 不再内置默认 OAuth 站点，OAuth 后端必须由你在 `settings.oauth` 中显式配置，并已支持 `manualRedirectUrl` 手动回调模式。
- OAuth token 现在优先写系统凭据库；只有系统凭据库不可用时才回退到用户配置目录下的 `credentials.json`，并在支持的平台上以受限权限落盘。
- `openyak doctor` 已提供本地只读的 config/auth/runtime 健康检查，可直接指出常见的设置缺口和修复方向。
- `openyak doctor` 现在会尊重全局 `--model`；用 OpenAI-compatible 环境变量跑 `openyak --model <openai-family-model> doctor` 时，会检查与 prompt / REPL / GitHub workflow 相同的活动模型鉴权路径。
- `openyak foundations [task|team|cron|lsp|mcp]` 与 `openyak /foundations [task|team|cron|lsp|mcp]` 已提供只读的 operator discovery surface，用于解释当前 Task / Team / Cron / LSP / MCP 五族的 tool membership 与边界，而不把它们伪装成更宽的控制面。
- 当前还支持从本地 `toolProfiles` 配置中选择命名 tool profile，通过 `openyak --tool-profile <name> ...` 收窄当前 REPL 或单次 prompt 的 permission mode / allowed-tools ceiling；这一层仍然是 process-local、session transcript-only，且 sandbox 额外约束只对 `bash` 生效。
- `/commit` 与 `/commit-push-pr` 已修复为不会被 `.openyak/settings.local.json`、`.openyak/sessions/` 这类本地文件阻塞。
- `/commit` 现在会基于完整 workspace status 和 `git diff --stat HEAD` 生成 Lore commit 草稿，不再只看已暂存变更。
- `/pr` 现在会基于当前分支相对默认分支的 diff 生成标题和正文，避免把纯工作区噪音误当成 PR 真值。
- 编译产物的子命令 `--help` 语义已统一，不再错误地执行实际动作。
- `/diff` 现在会正确显示未跟踪文件，同时继续排除 `.openyak/settings.local.json`、`.openyak/sessions/` 等本地状态噪音。
- 最近一轮 fresh release-binary CLI command-surface 巡检已在 `2026-04-10` 再次执行：本轮 47 步直接矩阵重新覆盖了顶层/子命令 help、直接命令、direct slash CLI、`server start --detach` / `status` / `recover` / `stop`、前台 `server --bind 127.0.0.1:0` 停服路径、打包后二进制 `--help`，以及 `login` / `onboard` 等环境依赖路径的受控失败；对应 smoke/regression suites 也已同步重跑。
- Python 对照层的 port session store 继续默认落在系统临时目录，并已明确拒绝 path-traversal / nested `session_id`，避免 `load-session` 或持久化读写越出会话目录。
- 已新增 mock parity harness 基础设施、Task/Team/Cron registry-backed tool foundations、LSP/MCP registry operator surfaces，以及更强的 tool-layer permission enforcement；其中 Task/Team/Cron registry 的 V1 contract 已冻结为进程内临时状态与 metadata-first 语义。
- 插件 manifest 的相对路径现在会做边界校验；解析后的路径必须保持在插件根目录内，不能再借由非字面量路径逃逸出插件目录。
- 插件配置路径现在也会做边界校验：`plugins.installRoot` / `plugins.registryPath` 必须留在用户级 config home 内，repo-scoped `plugins.externalDirectories` / `plugins.bundledRoot` 必须留在当前工作区内；managed plugin update/uninstall 也会拒绝越出 managed install root 的路径。
- `openyak server` 在 runtime/provider bootstrap 失败时，仍会把刚提交的 turn 或 user-input response 乐观写回线程状态，attach-first 客户端不会再因为这类早期失败而丢失用户输入痕迹。
- 内置 `REPL` tool 现在会真正执行 `timeout_ms`；超时后会终止子进程，返回 `exitCode=124`，并在 `stderr` 里附带明确的 timeout 文本，而不再静默忽略超时参数。
- Windows 下的命令解析、浏览器启动、`gh` 调用、构建、`clippy` 和测试链路都已收口到可维护状态。

## 仓库结构

- `assets/`：图片和文档素材
- `sdk/`：SDK 相关交付；当前包含本地 attach-first 的 TypeScript alpha 与 Python alpha
- `rust/`：主 Rust 工作区
- `src/`：Python 对照与审计工具
- `tests/`：Python 对照验证
- [`OPENYAK.md`](./OPENYAK.md)：本仓库自己的维护约定

需要特别说明：

- 本仓库当前使用 `OPENYAK.md` 作为仓库级维护文档。
- `openyak init` 给下游项目脚手架生成的也会是 `OPENYAK.md`。
- 当前代码、测试和 prompt discovery 已统一围绕 `OPENYAK.md` 展开。

`rust/` 下的主要 crate：

- `api`：模型 Provider 抽象、请求转换、流式响应处理
- `openyak-cli`：主二进制、REPL、渲染、初始化与命令分发
- `commands`：slash command、agents/skills 枚举、配置与信息展示
- `compat-harness`：兼容性与迁移辅助
- `lsp`：LSP 相关类型与上下文增强基础
- `plugins`：插件清单、注册表、工具聚合、hook 聚合与生命周期
- `runtime`：会话、权限、prompt、配置、OAuth、MCP 与运行时核心逻辑
- `server`：本地 HTTP/SSE 服务端 crate（由 `openyak server` 暴露为本地 thread/session server）
- `tools`：内置工具定义与执行实现

## 快速开始

### 1. 构建

```bash
cd rust
cargo build --workspace
```

### 2. 运行 CLI

```bash
cd rust
cargo run --bin openyak -- --help
cargo run --bin openyak -- --version
cargo run --bin openyak --
cargo run --bin openyak -- "总结当前工作区"
cargo run --bin openyak -- prompt "总结当前工作区"
cargo run --bin openyak -- --tool-profile audit prompt "只读审查当前工作区"
cargo run --bin openyak -- skills
cargo run --bin openyak -- skills available
cargo run --bin openyak -- agents
cargo run --bin openyak -- onboard
cargo run --bin openyak -- doctor
cargo run --bin openyak -- server --help
cargo run --bin openyak -- server start --detach --bind 127.0.0.1:0
cargo run --bin openyak -- server status
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

### 3. 初始化下游项目

```bash
cd rust
cargo run --bin openyak -- init
```

`openyak init` 当前会为目标工作区脚手架生成：

- `OPENYAK.md`
- `.openyak.json`
- `.openyak/`
- 推荐的本地 `.gitignore` 条目

## 认证与 GitHub 链路

OAuth 登录：

```bash
cd rust
cargo run --bin openyak -- login
```

`openyak login` 只会使用你在 `settings.oauth` 里显式配置的 OAuth 站点；仓库不再内置任何默认登录站点。仓库提供两份可直接填写的模板：

- loopback 回调版：[`rust/docs/oauth.settings.loopback.template.json`](./rust/docs/oauth.settings.loopback.template.json)
- 手动回调版：[`rust/docs/oauth.settings.manual-redirect.template.json`](./rust/docs/oauth.settings.manual-redirect.template.json)

最小配置示例：

```json
{
  "oauth": {
    "clientId": "your-client-id",
    "authorizeUrl": "https://auth.example.com/oauth/authorize",
    "tokenUrl": "https://auth.example.com/oauth/token",
    "manualRedirectUrl": "https://auth.example.com/oauth/callback"
  }
}
```

- 未配置 `manualRedirectUrl` 时，`openyak login` 默认监听本地 `http://localhost:<port>/callback` 回调
- 配置了 `manualRedirectUrl` 时，`openyak login` 会打开授权页，然后要求你手动粘贴最终回跳 URL 或 query string
- OAuth token 会优先写入系统凭据库；只有系统凭据库不可用时，才回退到用户配置目录下的 `credentials.json`
- 如果你想用本地 loopback 回调，可以保留 `callbackPort` 并删除 `manualRedirectUrl`

如果要在交互式 `openyak` 会话里运行 `/pr`、`/issue`、`/commit-push-pr` 等 GitHub 链路，请先确保本机已经完成：

```bash
gh auth login --web
```

- 这些是 REPL slash command，不支持 `openyak /pr ...` 这种 direct slash CLI 入口；请先启动 `openyak`
- 这三条链路不仅需要 `gh auth status` 成功，也需要当前活动模型具备可用鉴权，因为 `openyak` 会先起草 commit / PR / issue 文案，再调用 GitHub
- `openyak doctor` 现在会同时检查 GitHub CLI 可解析性、`gh auth status` 和活动模型本地 auth bootstrap，适合作为只读预检

`2026-04-08` 的一轮真实验收已经证明：只要给出 OpenAI-compatible gateway/API key，并显式选择 OpenAI-family model，当前实现就可以成功跑通 provider-backed `doctor` / `prompt` / 单轮 REPL，以及 disposable GitHub `/issue`、`/pr`、`/commit-push-pr`。

```bash
export OPENAI_BASE_URL="https://your-openai-compatible-gateway/v1"
export OPENAI_API_KEY="..."
cd rust
cargo run --bin openyak -- --model gpt-5.3-codex doctor
cargo run --bin openyak -- --model gpt-5.3-codex prompt "reply with the exact text: OPENYAK_PROMPT_OK"
printf 'reply with the exact text: OPENYAK_REPL_OK\n/exit\n' | cargo run --bin openyak -- --model gpt-5.3-codex
```

- `openyak login` 仍然是独立的 OAuth 链路；即使 API key 已经足够让 prompt / REPL / GitHub workflow 成功，只要 `settings.oauth.clientId`、`authorizeUrl`、`tokenUrl` 没配齐，`openyak login` 仍会直接拒绝执行。
- 推荐的 disposable GitHub 验收顺序是：临时私有仓库 `/issue` -> 手工 commit + push throwaway branch 后 `/pr` -> 回到 `main` 保留未提交改动后 `/commit-push-pr`。
- 清理时优先执行 `gh repo delete OWNER/REPO --yes`。如果当前 `gh` token 缺少 `delete_repo` scope，先用 `gh issue close` / `gh pr close -d` 清掉 issue、PR 和远端分支，再运行 `gh auth refresh -h github.com -s delete_repo` 补齐删除权限后重试仓库删除。

如果你想先做一次本地健康预检，可以直接运行：

```bash
cd rust
cargo run --bin openyak -- doctor
```

如果你想用显式、可重跑的本地向导把这些现有步骤串起来，也可以运行：

```bash
cd rust
cargo run --bin openyak -- onboard
```

`openyak onboard` 只在交互式本地终端里运行；它会先做只读 readiness assessment，再按需串联 `openyak init`、用户级默认模型写入、provider-aware auth guidance / `openyak login` handoff，以及最终的 `openyak doctor`。如果在非交互终端里调用，它会直接拒绝执行并明确说明原因。

## 配置、路径与 skills 规则

当前仓库已经把用户目录、配置目录和 skills 规则统一成一套平台感知逻辑：

- `OPENYAK_CONFIG_HOME` 最高优先，用来显式指定用户级 `.openyak` 目录
- 若未设置 `OPENYAK_CONFIG_HOME`，则读取 `CODEX_HOME`；它显式指定用户级 `.codex` 目录，并用其父目录推导默认用户根目录
- 若两者都未设置，则回退到平台默认用户目录
- 不再把当前工作目录下的 `./.openyak` 当作默认用户目录

平台默认用户目录的回退顺序：

- Windows：`USERPROFILE`，其次 `HOMEDRIVE` + `HOMEPATH`，最后才看 `HOME`
- macOS / Linux：`HOME`，其次 `USERPROFILE`，再其次 `HOMEDRIVE` + `HOMEPATH`
- 如果这些变量都不可用，运行时最后回退到系统临时目录，而不是当前工作目录

项目级配置仍然放在：

- `.openyak.json`
- `.openyak/settings.json`
- `.openyak/settings.local.json`

skills 目录同时支持两种布局：

- `skills/<name>/SKILL.md`
- `skills/.system/<name>/SKILL.md`

`openyak skills` 和 `Skill` 工具使用同一套枚举/解析逻辑，因此两种布局的结果一致。

OP5 phase-1 现在额外提供了一个 local-only 的 curated skills registry：

- `openyak skills available` 列出当前可安装的 curated skills catalog
- `openyak skills info <skill-id>` 查看已安装 provenance 和可用版本
- `openyak skills install <skill-id> [--version <x.y.z>]` 把 standard-placement skill 安装到用户级 managed root
- `openyak skills update <skill-id> [--version <x.y.z>]` 更新已安装的 registry-managed skill；显式版本会形成 exact-version pin
- `openyak skills uninstall <skill-id>` 卸载 registry-managed skill
- managed installs 固定落在 `<openyak-home>/skills/.managed`，不会改写手工维护的 project/user roots
- phase-1 只支持 registry-managed `standard` placement；手工 `.system` skills 仍然可发现，但不纳入 managed install/update/uninstall
- 如果没有显式传入 `--registry`，运行时会先看 `settings.skills.registryPath`，再回退到仓库自带的 `assets/skills/registry.json`

对应配置入口：

```json
{
  "skills": {
    "registryPath": "C:/path/to/skills/registry.json"
  }
}
```

插件路径规则：

- `settings.plugins.installRoot` 和 `settings.plugins.registryPath` 的相对路径按用户级 config home 解析，最终路径也必须保持在该目录内。
- user config 中的 `settings.plugins.externalDirectories` / `settings.plugins.bundledRoot` 非绝对路径默认相对 config home；如果显式写成 `./...`，则按当前 workspace 解析。
- project/local config 中的 `settings.plugins.externalDirectories` / `settings.plugins.bundledRoot` 一律按当前 workspace 解析，且不能通过 `..` 或其他方式逃逸出当前仓库。

“今天日期”相关内容由运行时按本地时钟动态生成，只允许在测试中注入固定日期，不再让仓库中的常量污染 system prompt 或状态输出。

## 当前可用能力

当前主实现已经具备以下能力面：

- 交互式 REPL、单次 prompt、会话保存、恢复、导出和工作区状态输出
- Shell/PowerShell、文件读写与编辑、搜索、Web fetch/search、TodoWrite、NotebookEdit、Skill、Agent、ToolSearch 等内置工具
- 可选的本地 browser tools：`BrowserObserve` 用于单次 rendered-page observe，`BrowserInteract` 用于单次 selector-backed click 后回传页面状态；两者都要求 `browserControl.enabled=true`、显式 `--allowedTools BrowserObserve` / `BrowserInteract` 和 `danger-full-access`，并继续保持 CLI-local、single-call、无持久 browser session、无 `/v1/threads` browser support
- registry-backed parity foundation/operator tools：TaskCreate/Get/List/Stop/Update/Output/Wait、TeamCreate/Get/List/Delete、CronCreate/Get/Disable/Enable/Delete/List，以及基础 LSP/MCP registry operator surface
- local-only 的 Session operator surface：`SessionList`、`SessionGet`、`SessionCreate`、`SessionSend`、`SessionResume`、`SessionWait`
- `/status`、`/compact`、`/model`、`/permissions`、`/cost`、`/config`、`/memory`、`/init`、`/diff`、`/version`、`/session`、`/resume`、`/plugin`、`/plugins`、`/debug-tool-call` 等 slash command
- `/branch`、`/worktree`、`/commit`、`/pr`、`/issue`、`/commit-push-pr` 等 Git/GitHub 工作流命令
- 这些 Git/GitHub 工作流命令在交互式 REPL 中可用；direct slash CLI 入口当前只覆盖 `/agents`、`/skills`、`/foundations`
- 插件发现、安装、启用、禁用、卸载、更新，以及插件工具和 hook 聚合
- `openyak prompt "..."`、`openyak agents`、`openyak skills [list|available|info <skill-id>|install <skill-id>|update <skill-id>|uninstall <skill-id>]`、`openyak foundations [task|team|cron|lsp|mcp]`、`dump-manifests`、`bootstrap-plan`、`system-prompt`、`login`、`logout`、`init`、`doctor`、`server`、`package-release` 等顶层命令
- `openyak /agents`、`openyak /skills`、`openyak /foundations [task|team|cron|lsp|mcp]` 这类 direct slash CLI 入口，以及 `openyak --resume SESSION.json ...` 的 resume-safe slash command 链路
- `openyak onboard` 作为显式、可重跑、local-only 的 phase-1 onboarding command，复用现有 init/login/doctor/config/provider surfaces，不拦截默认 REPL 启动
- MCP stdio、OAuth 及相关运行时能力；Task/Team/Cron registry 当前明确保持 `process_local_v1` contract，并稳定暴露 `created_at`、`updated_at`、`last_error`、`disabled_reason`、`origin`、`capabilities` 等 metadata

## 文档索引

- 仓库总览：[`README.md`](./README.md)
- 仓库维护约定：[`OPENYAK.md`](./OPENYAK.md)
- 贡献指南：[`CONTRIBUTING.md`](./CONTRIBUTING.md)
- 安全策略：[`SECURITY.md`](./SECURITY.md)
- 行为准则：[`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)
- 许可证：[`LICENSE`](./LICENSE)
- Rust 使用说明：[`rust/README.md`](./rust/README.md)
- Rust 工作区维护契约：[`rust/OPENYAK.md`](./rust/OPENYAK.md)
- Rust 贡献指南：[`rust/CONTRIBUTING.md`](./rust/CONTRIBUTING.md)
- Foundation parity/operator surface：[`rust/docs/parity-foundation-registries.md`](./rust/docs/parity-foundation-registries.md)
- Mock parity harness：[`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md)
- 发布说明草案：[`rust/docs/releases/0.1.0.md`](./rust/docs/releases/0.1.0.md)
- Python SDK README：[`sdk/python/README.md`](./sdk/python/README.md)
- TypeScript SDK README：[`sdk/typescript/README.md`](./sdk/typescript/README.md)

## 验证方式

最近一次本地全量验收（`2026-04-09`）已覆盖：

```bash
cd rust
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
python -m unittest discover -s tests -v
```

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

上述三条本地验证链路现在已经同步固化到 GitHub Actions [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)。CI 只复现这些当前可在仓库内真实执行的检查，不宣称 release/upload 已自动化完成。

本次文档刷新还额外补做了一轮在 `2026-04-10` 完成的 fresh release-binary 级直接命令矩阵与 CLI smoke/regression rerun，覆盖：

- `cargo build --manifest-path rust/Cargo.toml --workspace`
- `cargo build --manifest-path rust/Cargo.toml --release -p openyak-cli`
- `cargo test --manifest-path rust/Cargo.toml -p openyak-cli --test command_surface_cli_smoke --test doctor_cli_smoke --test server_cli_smoke --test onboard_cli_smoke --test package_release_cli_smoke`
- `pnpm --dir sdk/typescript build`
- `python -m build`（`sdk/python`）
- 47 个直接 release-binary help/命令/子命令步骤

其中这 47 个直接步骤再次覆盖了：

- `openyak --help`
- `openyak prompt --help`、`dump-manifests --help`、`bootstrap-plan --help`、`agents --help`、`skills --help`、`foundations --help`、`system-prompt --help`、`login --help`、`logout --help`、`init --help`、`onboard --help`、`doctor --help`、`package-release --help`、`server --help`
- `openyak dump-manifests`、`bootstrap-plan`、`agents`、`skills`、`foundations`、`foundations task`、`foundations mcp`、`system-prompt`、`logout`、`init`、`doctor`、`package-release`
- `openyak /agents`、`openyak /skills`、`openyak /foundations task`
- `openyak server start --detach --bind 127.0.0.1:0`、`openyak server status`、`openyak server recover`、`openyak server stop`
- `openyak server --bind 127.0.0.1:0` 的真实启动与停服路径
- 打包后二进制 `openyak(.exe) --help`
- `openyak login`、`openyak onboard` 的受控失败路径

其中 `openyak foundations lsp` 仍以前一轮直接复核结论为准：当前 detail 输出使用高层 `Tools            LSP` 标签，不再展开成单条 `LspGetDiagnostics` 文案。

其中需要交互、TTY 或外部认证条件的链路，文档口径以“受控失败且错误原因正确”为通过标准，而不是伪造成功。

如果修改涉及路径、OAuth、skills、插件 manifest、外部命令解析或 Git/GitHub 工作流，仅靠单元测试还不够。优先再做一次直接功能验证。

## 当前限制

- 当前已经支持通过 `openyak package-release` 生成 release artifact 目录，但压缩、上传和自动分发流程仍需继续完善。
- GitHub Actions CI 已固化当前本地验证基线，但 release artifact 上传、签名和自动分发流程仍需继续完善。
- 某些 live-provider 集成测试默认不启用，因为它们依赖真实外部凭据和网络环境。
- `openyak doctor` 当前只做本地只读预检，不提供自动修复、配置迁移或远程服务探测。
- Task/Team/Cron registry 的 V1 contract 仍然只提供进程内临时状态，不承诺跨进程持久化、恢复、租约或共享服务语义。
- `openyak server` 当前是 bind 范围限制在 loopback 地址的本地 thread/session server，不是 codex-style full app-server 或远程控制面。
- daemon/control-plane 路线仍未进入独立 operator plane 阶段：当前没有 daemon service install/start/stop/status/recover 的稳定 public surface，也没有远程 operator access。
- 当前补齐的 LSP/MCP 仍以 operator-facing bridge 为主；完整独立 LSP main entry 仍未作为稳定用户入口能力发货。
- `0.x` 阶段的命令面和交互细节仍可能继续演进。

## 后续优化方向

- 保持 registry V1 contract 稳定，优先继续补状态模型和低风险 operator-facing surface，而不是让 foundation registry 暗长成完整服务。
- Task / Team / Cron 优先继续补 `status` / `wait` / `history` / `enable` 这类可观测、低副作用接口，再评估更重的控制面。
- MCP / LSP 优先把 capability、status、diagnostics、resource/auth visibility 做实，再决定是否扩到更完整的独立 LSP 主入口或更宽的 server surface。
- `openyak server` 保持 local thread/session HTTP/SSE 语义，避免暗长成 codex-style app-server；完整 LSP main entry 继续保留为后续 milestone。
- 建立更完整的 Windows 发布与安装流程，并让 Python 对照工具继续聚焦于审计和迁移支持。

## 免责声明

- 本仓库不主张拥有原始上游源材料的权利。
- 本仓库不隶属于、也不代表原项目作者或其组织。
- 当前仓库的重点是清洁实现、接口研究、对照验证与开源工程化。
