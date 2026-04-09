# Registry-backed parity foundation 状态说明

本文只覆盖 `rust/` 内已经落地的 foundation / registry-backed parity slices，聚焦低风险、面向操作员的能力：`Task*`、`Team*`、`Cron*`、`LSP`、`MCP`。

非目标：

- 不把 `openyak server` 描述成 codex-style full app-server、远程控制面或完整服务平台
- 不把完整 LSP main entry / 真实语言服务器宿主描述成已对外 surfacing 完成
- 不扩展到历史 parity repo 的完整产品面

当前阶段的明确叙事是：

- 先把 foundation registry contract 冻结，再继续做 operator-facing surface。
- 先补状态模型和可观测接口，再决定是否扩到更重的控制面。
- 现在已经有 `openyak foundations [family]` / `openyak /foundations [family]` 作为只读 discovery surface，用于解释当前五族的 shipped boundary。
- `openyak server` 当前只提供窄化的本地 thread/session surface；更宽的 user-facing server / control-plane surfacing 仍放在后续 milestone。

## Registry V1 contract（已冻结）

当前 `TaskRegistry`、`TeamRegistry`、`CronRegistry` 的 V1 contract 明确冻结为：

- **仅进程内 / 临时状态**：对象只存在于当前 runtime 进程；新进程不会自动恢复旧状态。
- **不提供持久化、恢复、租约语义**：当前不承诺跨进程 durability、lease ownership、crash recovery、跨实例共享。
- **优先冻结 metadata，而不是继续堆入口**：`created_at`、`updated_at`、`last_error`、`disabled_reason`、`origin`、`capabilities` 这些字段是 V1 operator contract 的核心。

当前统一的 metadata 口径：

- `origin = "process_local_v1"`：明确表明 backing store 只是当前进程内 registry。
- `contract.truth_layer = "process_local_v1"` / `contract.operator_plane = "local_runtime_foundation_v1"` / `contract.persistence = "process_memory_only_v1"`：继续把 foundation lifecycle contract 锁在当前 runtime 进程，不借用 thread daemon contract。
- `openyak foundations ...` 输出里的 `Truth` / `Operator label` 行应继续把这些 family 标成 `process_local_v1 runtime-only truth` 或对应的 registry bridge，而不是借 thread daemon truth 做过度类比。
- `capabilities`：列出当前对象在 V1 中稳定可用的低风险 operator-facing 动作。
- `last_error`：仅记录该对象最近一次“命中对象本身的非法状态操作”错误；缺失并不代表系统从未报错，只代表当前对象没有保留到更具体的错误上下文。
- `disabled_reason`：当前只用于 cron，V1 固定记录低风险 disable 语义，而不是引入完整调度控制面。
- `created_at` / `updated_at`：统一作为 V1 object lifecycle metadata 暴露给 tool payload，避免 operator 只能依赖隐含顺序推断对象新旧。

当前仓库里已经有一组更窄、但已经结构化的 lifecycle / failure / recovery schema：

- thread snapshot 的 `contract` 层固定暴露 `truth_layer`、`operator_plane`、`persistence`、`attach_api`
- thread snapshot 的 `state` 层继续暴露 `status`、`run_id`、`recovery_note`
- 若线程因 server restart 等场景进入恢复态，还会附带 `recovery.failure_kind`、`recovery.recovery_kind`、`recovery.recommended_actions`

这组 schema 现在只服务于 `/v1/threads` attach-first 真值；它们不意味着 Task / Team / Cron 也具备 daemon-backed recovery recipe plane。

## 与 daemon-backed thread truth 的边界

当前仓库里已经有一个**更窄但确实 daemon-backed** 的 truth slice：`openyak server` 提供的 thread snapshot contract。

- thread snapshot 显式声明 `truth_layer = "daemon_local_v1"`
- thread snapshot 同时声明 `operator_plane = "local_loopback_operator_v1"` 与 `persistence = "workspace_sqlite_v1"`
- thread snapshot 同时声明 `attach_api = "/v1/threads"`
- restart 后的 `interrupted` + `recovery_note`，以及 `failure_kind` / `recovery_kind` / `recommended_actions` 只适用于这条 attach-first thread truth

这不应和 foundation registry 的 `process_local_v1` 混为一谈：

- `Task*` / `Team*` / `Cron*` payload 继续使用 `origin = "process_local_v1"`
- 它们仍然只表示当前 runtime 进程内的临时 registry state
- 当前没有 daemon-backed worker/task/team lifecycle store、租约、跨进程恢复或统一 recovery recipe plane

评审口径应保持成对出现：**thread truth 已经带 daemon label；foundation truth 仍然是 process-local label。**
这条分界线也应体现在 operator-facing 文档、CLI 帮助叙事和 SDK README 中，避免把 thread-level recovery primitive 误写成更宽的 daemon control plane。

## 当前已经落地的 operator-facing surface

### Task registry (`crates/runtime/src/task_registry.rs` + `crates/tools/src/lib.rs`)

当前工具层已经对外提供：

- `TaskCreate`
- `TaskGet`
- `TaskList`
- `TaskStop`
- `TaskUpdate`
- `TaskOutput`
- `TaskWait`

运行时侧由进程内 `TaskRegistry` 提供 backing：任务创建、状态推进、消息追加、输出缓冲、以及和 team 的关联。

评审结论：这部分已经足够支撑 parity foundation 层的 task lifecycle 演示与测试；V1 现在明确冻结为进程内状态，不做跨进程持久化，同时补上 `created_at` / `updated_at` / `origin` / `capabilities` / `last_error` 这类 metadata contract。

### Team / Cron registry (`crates/runtime/src/team_cron_registry.rs` + `crates/tools/src/lib.rs`)

当前工具层已经对外提供：

- `TeamCreate`
- `TeamGet`
- `TeamList`
- `TeamDelete`
- `CronCreate`
- `CronGet`
- `CronDisable`
- `CronEnable`
- `CronDelete`
- `CronList`

运行时侧已经具备但暂未全部对外 surfacing 的基础能力：

- `TeamRegistry::{remove}`
- `CronRegistry::{record_run}`

评审结论：当前暴露面仍然是刻意收敛过的 foundation slice，但已经从“只做 create/list/delete”推进到“补充只读查询、wait/disable/enable、以及 registry metadata”；更细的 team/cron 管理动作仍保留在 runtime 内部，为后续 staged surfacing 留出空间。

### LSP registry (`crates/runtime/src/lsp_client.rs` + `crates/tools/src/lib.rs`)

当前 `LSP` 工具是 registry-backed 查询入口，而不是完整 LSP 产品面。现状包括：

- 列出当前 registry 中已注册的 LSP servers
- 查询 registry server 的 status / root path / capabilities / diagnostic count
- 按文件扩展名把请求路由到已注册 language server
- 查询缓存 diagnostics
- 对 `hover` / `definition` / `references` / `symbols` 等动作返回结构化 dispatch 结果
- 对未注册或未连接 server 给出明确错误

评审结论：这已经满足 parity foundation 的“有 registry、有查询入口、有 status/capabilities/diagnostics 语义、有错误语义”目标，但仍然明确停留在 runtime-backed bridge 层；当前已发货的 `openyak server` 也不改变这一点，完整独立 LSP main entry 仍不应被当作当前稳定用户入口。

### MCP bridge (`crates/runtime/src/mcp_tool_bridge.rs` + `crates/tools/src/lib.rs`)

当前工具层已经对外提供：

- `ListMcpServers`
- `ListMcpTools`
- `ListMcpResources`
- `ReadMcpResource`
- `McpAuth`
- `MCP`

运行时侧由 `McpToolRegistry` 持有已连接 server 的状态、resources、tools 和 auth 状态；实际工具调用继续桥接到现有 `McpServerManager`。当前 `ListMcpServers` / `McpAuth` 也已经把 auth state、tool/resource/prompt visibility 与计数信息暴露给 operator。

评审结论：这条链路已经不是 stub，而是可测试、可枚举、可调用的 registry-backed bridge。当前主要边界在于：只有连接完成且 manager 已配置时，tool/resource surface 才能继续向前执行；它还不是更宽的 server/operator control plane。

## 代码质量结论（当前阶段）

本轮 review 聚焦“foundation slice 是否足够稳、接口是否刻意收敛、文档是否把边界讲清楚”，结论如下：

1. `runtime` 与 `tools` 的职责边界清楚：registry 存状态，tool 只做 schema + dispatch。
2. 现有测试已经覆盖 task/team/cron/lsp/mcp 的基础行为、错误路径和一部分权限约束。
3. 当前最需要补的是 operator-facing 文档、状态可见性和 staged follow-up 说明，而不是再引入一批高风险 API。
4. `openyak server` 应保持“窄化的本地 thread/session surface”叙事，完整 LSP 主入口仍应保持 unsurfaced，避免文档过度承诺。

## 建议的 staged follow-ups

按风险从低到高，建议后续继续追平时使用下面顺序：

1. **保持 V1 contract 稳定，而不是暗长成服务**：只有当上层真实工作流开始依赖跨进程 durability 时，再单独设计持久化、恢复、租约、共享语义；不要在 foundation registry 上隐式追加。
2. **Task / Team / Cron 继续补低风险 operator 面**：优先评估 `status` / `wait` / `history` / `enable` 这类接口，而不是先推更重的 orchestration / scheduler control plane。
3. **让 LSP bridge 逐步落到真实调用**：先在现有 registry-backed contract 上继续增加 status / capability / diagnostics 真实性，再决定是否需要把更完整的 LSP 宿主接进主 CLI。
4. **继续扩 MCP operator docs 与验证**：当前已经有 server/tool/resource 枚举，以及基础 auth state / capability visibility；后续围绕 manager 配置、连接状态、auth-required 场景补更直接的 operator 文档与测试，而不是立即扩张到更宽的 server surface。
5. **最后再考虑更宽的 user-facing surfacing**：`openyak server` 已经提供窄化的本地 thread/session 入口；当 registry contract 和 operator surface 足够稳定后，再评估是否需要更完整的 server surface 或独立 LSP main entry。

## 推荐验证方式

针对当前 foundation slice，优先看这些验证：

```bash
cargo test -p mock-anthropic-service
cargo test -p runtime
cargo test -p tools
cargo test -p openyak-cli --test mock_parity_harness -- --nocapture
python scripts/run_mock_parity_diff.py
```

如果要做更严格的工作区级收口，再补：

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
```
