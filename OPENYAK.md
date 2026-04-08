# OPENYAK.md

本文件是本仓库自己的维护契约。

当前约定已经统一：

- 本仓库使用 [`OPENYAK.md`](./OPENYAK.md) 作为仓库级维护说明。
- `openyak init` 为下游项目脚手架生成的也是 `OPENYAK.md`。
- 当前代码、测试和运行时提示发现逻辑都围绕 `OPENYAK.md` 展开。

## 仓库事实

- `rust/` 是当前唯一按产品标准维护的主实现。
- `src/` 与 `tests/` 是 Python 对照、审计和迁移辅助层，不代表主运行时行为。
- 当前持续维护的核心 Markdown 包括：根目录 [`README.md`](./README.md) / [`OPENYAK.md`](./OPENYAK.md)，`rust/` 下的 [`README.md`](./rust/README.md)、[`OPENYAK.md`](./rust/OPENYAK.md)、[`CONTRIBUTING.md`](./rust/CONTRIBUTING.md)、[`MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md)、[`docs/parity-foundation-registries.md`](./rust/docs/parity-foundation-registries.md)、[`docs/releases/0.1.0.md`](./rust/docs/releases/0.1.0.md)，以及两个 SDK README。
- 临时验收目录、生成的 agent 文档、会话导出文件和构建产物不属于正式文档面，不要把它们当成需要长期维护的说明文档。

## 单一真相来源

- 产品行为与运行时事实：`rust/` 下代码。
- 仓库总览与项目定位：[`README.md`](./README.md)。
- 仓库维护约定：[`OPENYAK.md`](./OPENYAK.md)。
- Rust 使用说明：[`rust/README.md`](./rust/README.md)。
- Rust 工作区维护契约：[`rust/OPENYAK.md`](./rust/OPENYAK.md)。
- Rust 贡献约束：[`rust/CONTRIBUTING.md`](./rust/CONTRIBUTING.md)。
- Foundation parity / operator surface 边界：[`rust/docs/parity-foundation-registries.md`](./rust/docs/parity-foundation-registries.md)。
- mock parity harness 使用说明：[`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md)。
- 发布说明草案：[`rust/docs/releases/0.1.0.md`](./rust/docs/releases/0.1.0.md)。
- SDK 对外使用面：[`sdk/python/README.md`](./sdk/python/README.md)、[`sdk/typescript/README.md`](./sdk/typescript/README.md)。

实现发生变化时，必须同步检查这些文档是否仍然准确，不要让 README、SDK 文档和真实代码再次分叉。

## 必须遵守的实现规则

### 路径与环境变量

涉及用户目录、配置目录、 skills、OAuth 或远程相关路径时，统一遵循共享 helper 的规则：

- `OPENYAK_CONFIG_HOME` 优先决定用户级 `.openyak` 目录。
- 否则读取 `CODEX_HOME`，并用其父目录推导默认用户根目录。
- 否则回退到平台默认用户目录。
- 不要再在新代码里手写“只看 `HOME`”或“默认写到当前工作目录 `./.openyak`”的逻辑。

平台用户目录的回退顺序：

- Windows：`USERPROFILE`，其次 `HOMEDRIVE` + `HOMEPATH`，最后 `HOME`。
- macOS / Linux：`HOME`，其次 `USERPROFILE`，再其次 `HOMEDRIVE` + `HOMEPATH`。

### OAuth

`openyak login` 相关逻辑必须保持下面这些约束：

- 不要再恢复任何内置默认 OAuth 站点。
- 运行时只使用 `settings.oauth` 中显式配置的 `clientId`、`authorizeUrl`、`tokenUrl` 等参数。
- OAuth 后端必须由使用者自己提供；不要在代码、文档或测试里偷偷回填默认服务 URL。
- 配置了 `manualRedirectUrl` 时，必须走手动回调输入模式，不再悄悄绑定本地 `localhost`。
- token 必须优先写系统凭据库；只有系统凭据库不可用时，才允许回退到用户级 `credentials.json`。

### Skills

skills 发现和解析必须复用共享实现，且同时支持：

- `skills/<name>/SKILL.md`
- `skills/.system/<name>/SKILL.md`

不要在 `commands`、`tools`、CLI 或测试里再维护一份私有扫描逻辑，也不要引入任何机器本地硬编码路径。

### 插件路径边界

插件 manifest 中声明的文件和目录路径必须继续满足下面这些约束：

- 路径必须相对插件根目录解析。
- 解析后的最终路径必须保持在插件根目录内。
- 不要为了“方便”重新放开 `..` 逃逸、绝对路径直通或其他会越出插件根目录的写法。

插件配置路径也必须继续满足下面这些约束：

- `settings.plugins.installRoot` 和 `settings.plugins.registryPath` 的最终路径必须保持在用户级 config home 内。
- project/local config 中的 `settings.plugins.externalDirectories` 和 `settings.plugins.bundledRoot` 必须保持在当前 workspace 内。
- managed plugin update/uninstall 只能作用在 managed install root 下的插件路径上。

文档里要把这件事描述为路径边界加固，而不是夸大成完整沙箱。

### 日期与时钟

任何“今天”“当前日期”“最近”相关运行时行为，都必须由时钟在执行时生成。只有测试可以注入固定日期。实现里不要再引入影响 system prompt 的硬编码日期常量。

### 外部命令解析

凡是需要调用系统命令的链路，尤其是 Windows 下的 `gh`、浏览器启动器或 shell 程序，都应优先走共享命令解析 helper，而不是直接硬编码程序名。

### Git 本地状态

涉及 `/commit`、`/commit-push-pr` 或其他自动 staging 行为时，必须保证本地 `.openyak/settings.local.json`、`.openyak/sessions/` 之类工作区本地状态不会阻塞命令，也不能被意外提交。

### CLI help、server 与 diff 语义

- 顶层子命令的 `--help` 应返回对应命令的专属帮助，而不是误执行命令主体或退回根 help。
- `/diff` 必须如实反映当前工作区变化；除了明确排除的本地状态文件外，不要漏掉未跟踪文件。
- `openyak server` 当前是已经 surfaced 的本地 HTTP/SSE thread server 入口；文档必须描述它暴露 `/v1/threads` 和 legacy `/sessions` compatibility routes，并把状态持久化到工作区 `.openyak/state.sqlite3`。
- 同时也必须明确：`openyak server` 不是 codex-style full app-server 或远程控制面。

### Registry-backed parity foundation

涉及 Task / Team / Cron / LSP / MCP foundation slices 时，统一遵循下面这些边界：

- Task / Team / Cron 的 V1 contract 固定为 `process_local_v1`，只表示进程内临时状态。
- 不要在未经过单独设计的情况下，偷偷引入持久化、恢复、租约、共享服务或 crash recovery 语义。
- 优先维护稳定 metadata：`created_at`、`updated_at`、`last_error`、`disabled_reason`、`origin`、`capabilities`。
- `LSP` 当前是 registry-backed query / dispatch bridge，不是完整 LSP main entry。
- `server` crate 已通过 `openyak server` 暴露窄化的本地 thread/session surface，但不要把它写成完整控制面。

## 文档同步规则

当你修改下面这些内容时，必须同步更新对应 Markdown：

- 产品定位、能力边界、主实现归属：更新根目录文档与 [`rust/README.md`](./rust/README.md)。
- 路径、环境变量、OAuth、skills、日期规则：同时更新根目录和 Rust 文档。
- `openyak server` 的路由、持久化位置或 public boundary：同时更新根 README、Rust README、Python SDK README 和 TypeScript SDK README。
- SDK 的包名、导入名、协议边界或验证方式：同步更新对应的 Python SDK README 或 TypeScript SDK README。
- `openyak init` 的脚手架产物：同步更新根 README，并保持 `OPENYAK.md` 口径一致。
- release binary、packaged-use 或平台相关示例：至少同时覆盖类 Unix 与 Windows PowerShell 写法，不要让 README 只剩单平台命令。
- 插件 manifest 或插件配置路径边界、权限语义：同步更新根 README、Rust README 和发布说明。
- 发布能力或版本说明：同步更新发布说明草案。
- Registry-backed parity foundation / operator surface 边界：同步更新 [`rust/docs/parity-foundation-registries.md`](./rust/docs/parity-foundation-registries.md)。
- mock harness 的运行方式、场景或输出契约：同步更新 [`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md)。

## 验证要求

Rust 行为改动至少执行：

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
```

如果改动影响 Python 对照层，再补跑：

```bash
python -m unittest discover -s tests -v
```

如果改动影响 SDK，再分别补跑：

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

如果只是文档改动，至少做一次链接/引用校验和 diff 自查，避免继续保留失效路径或陈旧命令。

## 提交与维护约定

- 优先做聚焦、可审阅的改动，不要把无关重构和行为修复混在一起。
- 修改仓库行为后，先自查文档是否过时，再提交。
- 共享默认配置优先放在 `.openyak.json` 或 `.openyak/settings.json`；`.openyak/settings.local.json` 只用于机器本地覆盖。
- 如果同时触及 Rust 主实现和 Python 对照层，必须保持叙事一致，不要重新制造“文档写一套、代码跑另一套”的问题。
- 自己生成的临时验收目录、临时私有仓库引用、会话导出文件和构建产物，在任务结束前应尽量清理，不要把噪音留在仓库里。
