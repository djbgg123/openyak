# 贡献指南

本文档说明如何为 `rust/` 工作区提交可维护、可验证的改动。

## 开发环境

- 安装 Rust stable 工具链。
- 在 `rust/` 目录内执行构建和验证命令。
- 如果改动依赖模型调用、OAuth 或 GitHub 工作流，请准备相应的真实外部条件。

## 修改前先确认的边界

提交改动前，先确认自己修改的是哪一层：

- 主产品行为：看 `rust/` 下代码和文档。
- 对照、审计、迁移辅助：看仓库根目录的 `src/` 与 `tests/`。

不要把 Python 对照层重新写成主运行时，也不要把 Rust 主实现降格成“仅供参考”。

## 必须遵守的行为规则

### 用户目录与配置目录

- `OPENYAK_CONFIG_HOME` 优先决定用户级 `.openyak` 目录。
- 若未设置，再读取 `CODEX_HOME`；它显式决定用户级 `.codex` 目录，并用其父目录推导默认用户根目录。
- 若两者都未设置，则回退到平台默认用户目录。
- 禁止把当前工作目录的 `./.openyak` 当作默认用户目录。

平台默认用户目录的回退顺序：

- Windows：`USERPROFILE`，其次 `HOMEDRIVE` + `HOMEPATH`，最后 `HOME`
- macOS / Linux：`HOME`，其次 `USERPROFILE`，再其次 `HOMEDRIVE` + `HOMEPATH`

### OAuth

`openyak login` 相关逻辑必须保持下面这些约束：

- 不要再恢复任何内置默认 OAuth 站点。
- 运行时只使用 `settings.oauth` 中显式配置的 `clientId`、`authorizeUrl`、`tokenUrl` 等参数。
- OAuth 后端必须由用户配置；不要在 Rust 代码、默认配置或测试夹具里引入隐式默认 URL。
- 如果 `settings.oauth.tokenUrl` 或 provider `baseUrl` 指向 `localhost` / loopback IP，相关 HTTP 客户端必须绕过继承的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`；不要让本地 token exchange 或本地 gateway 请求被宿主代理截走。
- 配置了 `manualRedirectUrl` 时，必须进入手动回调输入模式。
- token 必须优先写系统凭据库；只有系统凭据库不可用时，才允许回退到用户级 `credentials.json`。
- 覆盖 loopback OAuth refresh / 本地 provider 请求的测试应显式锁定这条 no-proxy 约束，避免回归时只在带代理的宿主环境里暴露故障。

### Skills

skills 必须同时支持：

- `skills/<name>/SKILL.md`
- `skills/.system/<name>/SKILL.md`

`commands`、`tools`、CLI 与测试都应复用共享的发现/解析逻辑。不要再写一套私有扫描器，也不要引入任何机器本地绝对路径。

### 日期

运行时逻辑里的“当前日期”必须由时钟动态生成。只有测试可以注入固定日期。任何会污染 system prompt 或状态输出的硬编码日期常量都应被视为缺陷。

### Git 本地状态

如果你的改动会影响自动 staging 或 GitHub 工作流命令，必须保证 `.openyak/settings.local.json`、`.openyak/sessions/` 之类本地文件不会阻塞 `/commit`、`/commit-push-pr` 等命令，也不会被意外提交。

### CLI 可执行语义

- 顶层子命令的 `--help` 必须输出对应命令的专属帮助，不要误触发真实动作。
- `/diff` 必须覆盖未跟踪文件，除非它们属于明确排除的本地状态目录/文件。

### 外部命令调用

如果你要在 Rust 里调用外部程序，尤其是 Windows 下的 `gh`、浏览器启动器、shell 或其他系统命令，请优先使用共享命令解析 helper，而不是直接写死程序名。

需要重点避免的写法包括：

- `Command::new("gh")`
- `Command::new("explorer")`
- `Command::new("rundll32")`

### Registry-backed parity foundation

如果你的改动触及 Task / Team / Cron / LSP / MCP foundation slices，必须遵守下面这些边界：

- Task / Team / Cron 的 V1 contract 当前固定为 `process_local_v1`，只表示进程内临时状态。
- 未经单独设计，不要偷偷引入持久化、恢复、租约、共享服务或 crash recovery 语义。
- 优先维护稳定 metadata：`created_at`、`updated_at`、`last_error`、`disabled_reason`、`origin`、`capabilities`。
- `LSP` 当前是 registry-backed query / dispatch bridge，不是完整 LSP main entry。
- `server` crate 已通过 `openyak server` 暴露为本地 thread/session HTTP/SSE 入口；实现和文档都必须明确它提供 `/v1/threads` 与 legacy `/sessions` compatibility routes，并把状态持久化到工作区 `.openyak/state.sqlite3`。
- 同时也不要把 `openyak server` 写成 codex-style full app-server 或远程控制面。

## 测试与回归要求

如果你的改动触及下列领域，请补对应回归测试，避免问题重复出现：

- Windows `HOME` 为空但 `USERPROFILE` 存在的路径解析场景
- `CODEX_HOME` 下 `.codex/skills/.system/*` 的发现
- `Skill` 工具对嵌套技能目录的解析
- 日期由时钟注入，而不是运行时常量
- OAuth 的系统凭据库优先级与手动回调模式
- 顶层子命令 `--help` 的行为一致性
- `/diff` 对未跟踪文件的展示
- Windows 外部命令解析与浏览器/GitHub CLI 链路
- Git 本地 `.openyak` 文件对 `/commit`、`/commit-push-pr` 的影响
- Task / Team / Cron registry metadata 与 `process_local_v1` contract
- LSP status/capabilities/diagnostics 与 MCP auth/resource/tool visibility

## 构建与验证

提交 PR 前，至少运行完整的 Rust 验证集：

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

## GitHub Actions 基线

公开仓库的 CI 基线位于 [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml)。

它当前镜像四条已经在仓库内接受并可本地复现的验证链路：

- Rust 工作区
- 根目录 Python 对照层
- Python SDK
- TypeScript SDK

这条工作流用于保证公开协作中的构建、测试、lint 和打包检查持续可重复；它不代表 release 上传、签名或自动分发已经建立。

如果改动影响真实交互链路，仅跑测试还不够。优先再做一次直接功能验证，例如：

- `cargo run --bin openyak -- --help`
- `cargo run --bin openyak -- --version`
- `cargo run --bin openyak -- skills`
- `cargo run --bin openyak -- skills --help`
- `cargo run --bin openyak -- agents`
- `cargo run --bin openyak -- server --help`
- `cargo run --bin openyak -- login`
- `cargo run --bin openyak -- prompt "..."`
- `.\target\release\openyak.exe --help` 或 `./target/release/openyak --help`
- `.\target\release\openyak.exe server --bind 127.0.0.1:0` 或 `./target/release/openyak server --bind 127.0.0.1:0`
- 相关的 Git/GitHub slash command

## 文档同步要求

你修改下面这些内容时，必须同步更新文档：

- 产品定位、能力边界、主实现归属：更新根目录文档
- 路径、环境变量、OAuth、skills、日期规则：同时更新根目录与 `rust/` 文档
- CLI 能力面、构建方式、平台说明：至少更新 `README.md` 与 `rust/README.md`
- `openyak server` 的路由、持久化位置或对外边界：同时更新根 README、`rust/README.md` 和 SDK README
- SDK 的包名、导入名、协议边界或验证方式：同步更新对应 SDK README
- 发布能力或版本摘要：同步更新发布说明
- registry-backed parity/operator surface 边界：同步更新 `docs/parity-foundation-registries.md`
- mock harness 的运行方式、场景或输出契约：同步更新 `MOCK_PARITY_HARNESS.md`
- release binary、打包或平台差异示例：同时补齐类 Unix 与 Windows PowerShell 写法

## Pull Request 要求

- 每个 PR 尽量只解决一个清晰的问题。
- 描述中应包含：改动动机、实现摘要、实际执行过的验证命令。
- 行为有变化时，在同一个 PR 中补上或更新测试。
- 请求 review 前，先确认代码、测试和文档已经同步收口。
