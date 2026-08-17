<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` 方法清单

所有方法都使用 JSON-RPC 2.0 request/response。表中的 `C→S` 是 Rust/Tauri
client 到 Java/AgentScope server，`S→C` 是反向 request；notification 不带
`id`，详见 [`events.md`](./events.md)。未知方法返回 `METHOD_NOT_FOUND`，未知字段
在 minor 版本内保留并忽略，不能改变已知字段语义。

## Runtime

| 方法 | 方向 | params 要点 | 成功语义 |
| --- | --- | --- | --- |
| `initialize` | C→S | major/minor、clientVersion、capabilities、limits | 返回最终版本、`serverInstanceId`、能力和 limits；未兼容则关闭 |
| `version` | C→S | `includeBuild?` | 返回 Java/AgentScope/Solon/Native Image/OS 构建信息 |
| `capabilities/read` | C→S | `includeUnavailable?` | 返回可用、已禁用和明确 unsupported 的能力 |
| `health/read` | C→S | `verbose?` | 返回 sidecar、DB、队列、AgentScope、Tool/MCP 健康 |
| `diagnostics/read` | C→S | `includeLogs?`、`maxBytes?` | 返回脱敏诊断；不含 secret、token、完整 prompt 或源码 |
| `shutdown` | C→S | `reason?`、`deadlineMs?` | 仅 ACK 已开始关闭；Java 收口后 stopped 并 EOF |

`initialize` 是唯一允许在 `initialized` 前执行业务协商的入口。`shutdown`、协议版本
错误或 EOF 都必须释放 pending request、审批、Tool/MCP 子进程和
数据库锁。Java 日志只能写 stderr/文件。

`initialized` 与 `runtime/statusChanged(ready)` 是握手通知而非额外 method：Rust 在
`initialized.params.readyToken` 发送当前 generation 的一次性 128-bit challenge，Java
处理后必须在 `ready` 状态中原样回显。缺失、错误、旧或重复 token 都是
`HANDSHAKE_FAILED`，不能以 `ready` 状态继续；非 ready runtime 状态不携带 token。

## Workspace 与 Thread

| 方法 | 方向 | params 要点 | 副作用/幂等边界 |
| --- | --- | --- | --- |
| `workspace/open` | C→S | `workspaceId`、`rootPath`、`trust` | canonicalize 后登记；不创建/删除用户目录 |
| `workspace/list` | C→S | `includeArchived?` | 只读查询 |
| `workspace/trust/set` | C→S | `workspaceId`、`trust` | 记录新的信任 revision；不会赋予 full access |
| `workspace/unregister` | C→S | `workspaceId` | 只移除 JA 记录，不删除用户目录 |
| `thread/create` | C→S | `workspaceId`、`title?`、`profileRevision?` | 返回新 `threadId`；重复客户端 key 不重复创建 |
| `thread/list` | C→S | `workspaceId`、分页参数 | 只读查询 |
| `thread/read` | C→S | `threadId`、`view=snapshot`、`afterSeq?`、`limit?` | 返回同一事务的 `snapshot` 与 `snapshotSeq` |
| `thread/subscribe` | C→S | `threadId`、`fromSeq?`、`subscriptionId?` | 登记 live stream；事件仍由 notification 发送 |
| `thread/unsubscribe` | C→S | 同一订阅字段 | 幂等移除，不改变 Thread |
| `thread/archive` | C→S | `threadId` | 软归档；active Turn 时拒绝或先取消 |
| `thread/delete` | C→S | `threadId` | 软删除，可恢复；不删用户 Workspace |
| `thread/purge` | C→S | `threadId` | 明确确认后只永久删除 JA 自有会话数据 |

同一 Thread 的 Turn 串行，不同 Thread 可并行；`thread/read` 返回已提交状态。snapshot 后的 live 订阅必须遵循 protocol.md 的 buffer→snapshot→排空→
live 流程。

## Turn 与 Agent 控制

| 方法 | 方向 | params 要点 | 成功语义 |
| --- | --- | --- | --- |
| `turn/start` | C→S | `threadId`、文本 input、`accessMode`、`profileRevision` | 仅 accepted/queued；终态由 `turn/completed` 表达 |
| `turn/cancel` | C→S | `threadId`、`turnId`、reason | ACK 收到取消；不代表进程已停止 |

`accessMode` 是 `read_only`、`workspace`、`full_access`。Shell 默认询问，用户可选择本次或会话放行。
`turn/start` 的 `clientRequestKey`、Thread/Turn 状态和 profile
revision 共同形成幂等边界；结果未知时客户端不能自动重放有副作用请求。

## Profile 与 Model

| 方法 | 方向 | params 要点 | 首发限制 |
| --- | --- | --- | --- |
| `profile/list` | C→S | 空对象 | 列出可用不可变 profile revision |
| `profile/read` | C→S | `profileRevision` | 返回来源、模型、Skill/MCP revision 和最终权限 |
| `profile/save` | C→S | `profile`、`expectedRevision?` | 原子保存；冲突返回 `CONFLICT` |
| `profile/activate` | C→S | `profileRevision` | 激活新 revision，不改变历史 Turn |
| `model/probe` | C→S | provider/model/protocol/credential ref | 只做能力探测，secret 通过 `secret/resolve` 获取 |
| `model/capabilities/read` | C→S | 空对象 | 返回 vision、tool、thinking 等实际探测结果 |

首发 `model.protocol` 只允许 `anthropic_messages` 与 `openai_chat_completions`。
OpenAI Responses API 不在 v1；UI 不得把它显示为可用。

## Skills

| 方法 | 方向 | params 要点 | 边界 |
| --- | --- | --- | --- |
| `skill/list` | C→S | 空对象 | 返回来源、描述、hash、scope、enabled、health |
| `skill/import` | C→S | builtin/directory/archive source | 校验 `SKILL.md`、路径、大小、编码、hash；不执行脚本 |
| `skill/enable` | C→S | skill revision、enabled、scope | 只切换已校验的显式 revision；失败不改变当前启用状态 |
| `skill/reload` | C→S | skill revision 可选 | 重新校验并生成 revision |
| `skill/health/read` | C→S | skill revision 可选 | 返回明确 degraded/invalid，不静默降级 |

首发没有远程市场、依赖求解、安装脚本、动态 Java/JS 注入或插件 UI。Skill 内容是
低信任上下文，脚本只有后续进入正常 Tool/Approval/Sandbox 链路才允许执行。

## MCP Tools

| 方法 | 方向 | params 要点 | 首发限制 |
| --- | --- | --- | --- |
| `mcp/list` | C→S | 空对象 | 列出健康、协议、transport、revision 和 unsupported 原因 |
| `mcp/save` | C→S | name/transport/endpoint/protocol/auth/args/env/headers/queryParams | stdio endpoint 只能是 executable/path；HTTP endpoint 无 userinfo/credential query；secret 只能通过 credentialRef |
| `mcp/delete` | C→S | `mcpRevision` | 先撤销 Tool revision，再回收连接 |
| `mcp/test` | C→S | `mcpRevision`、认证时 `profileRevision?` | tools/list 健康探测，不自动执行副作用 Tool；认证 server 必须携带 profileRevision |
| `mcp/reload` | C→S | `mcpRevision` | 有界重连/熔断，旧 revision 可回退 |
| `mcp/tools/read` | C→S | `mcpRevision` | 返回 namespaced Tool schema 和 policy |
| `mcp/toolPolicy/set` | C→S | revision/toolName/policy | 新 Tool 默认 ask；不信任 server 自报 read-only |

首发只支持 MCP `tools/list`、`tools/call`，stdio 与 Streamable HTTP、无认证或 OS
credential `static secret-ref`。stdio 认证使用 named env，HTTP 认证使用 bearer 或 named
custom header；顶层 `credentialRef` 仅为 HTTP bearer shorthand 且 deprecated。OAuth、Resources、Prompts、Sampling、Roots、
Elicitation、Apps 和插件市场返回稳定 unsupported。

## Input boundary

首版输入仅支持有界文本；图片和二进制输入后置到独立版本，不增加额外方法。

## Java→Rust requests

| 方法 | 关联 | response 允许的 result 核心字段 | 失败边界 |
| --- | --- | --- | --- |
| `approval/request` | thread/turn/item/approval | `decision`、`resolvedAt` | exactly-once；迟到/重复/未知 fail-closed |
| `secret/resolve` | profile/mcp revision | opaque `secretValue`（敏感，仅内存） | 不进 React、日志、trace、error、argv/env |
| `externalTool/request` | optional thread/turn/item | `accepted`、`output` | 仅协商能力，不能代理 Agent Shell |

这三个方法的 request ID 必须为 `s:`。Rust 永远不另发 `approval/respond`，也不把
`approval/requested/resolved` notification 当成决定通道。
