<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA `ja-rpc/v1` 协议

本目录冻结 JA 首发桌面端与 Java sidecar 之间的私有协议。它是
JSON-RPC 2.0 envelope 加 UTF-8、LF 分隔的 JSONL 全双工传输，**不是**
Codex app-server、ACP、WebSocket 或通用插件协议。可机器校验的根 Schema
是 [`schema/ja-rpc-v1.schema.json`](./schema/ja-rpc-v1.schema.json)，方法、结果、事件和
稳定错误码分别见 [`methods.md`](./methods.md)、[`results.md`](./results.md)、
[`events.md`](./events.md) 和 [`errors.md`](./errors.md)。

## 1. 传输与 frame

- Rust/Tauri 是 client，Java/AgentScope 是 server；两边都可以发 request、response 和 notification。
- stdin/stdout 使用 UTF-8 JSONL；每行恰好一个 JSON object，行尾是一个 LF（最后一个 frame 也必须有 LF）。JSON 字符串内的换行必须转义为 `\\n`。
- stdout 只能写协议 frame。日志、诊断、Solon/AgentScope 输出只能写 stderr 或轮转文件；任何无法解析为合法 frame 的 stdout 内容都视为协议故障。
- 每端各自维护永久 reader、single-writer queue 和 pending request registry。业务线程不能直接写 stdout 或另一端的 stdin。
- reader 先做 UTF-8、LF、JSON object、frame bytes 和 envelope 结构校验，再把工作交给有界 dispatcher；reader 不等待模型、数据库、Tool 或审批。
- `maxFrameBytes` 以 UTF-8 JSON bytes 计，不含末尾 LF；协商值必须在 1 KiB 至 16 MiB 内，首发基线为 `4194304`。
- 大命令输出、Diff、图片和二进制必须遵守协商的 inline 上限并带截断标记；v1 不定义产品级内容句柄。

协议 frame 不携带明文认证信息。它运行在同一 Tauri 进程创建的匿名 pipe 上；“同机管理员或调试器无法读取进程内存”不属于本协议的安全承诺。

## 2. Envelope 与方向

### Request

```json
{"jsonrpc":"2.0","id":"c:req-1","method":"turn/start","params":{"threadId":"thr_demo","input":[{"type":"text","text":"检查测试"}],"accessMode":"read_only","profileRevision":"profile_demo"}}
```

`id` 必须存在且是全局唯一的有限字符串。Rust 发起的 request 使用 `c:` 前缀，Java 发起的 request 使用 `s:` 前缀；两端只接受自己 pending registry 中登记过的 response。请求 ID 最大 98 字节。

### Response

```json
{"jsonrpc":"2.0","id":"c:req-1","result":{"accepted":true,"turnId":"turn_demo"}}
```

response 必须恰好包含 `result` 或 `error` 之一，且回显原 request 的 `id`。成功 response 只代表请求已接受、排队或决定已记录；长期状态、流式内容和最终成功不能从 response 推断，必须看权威事件。

### Notification

```json
{"jsonrpc":"2.0","method":"item/delta","params":{"serverInstanceId":"srv_demo","threadId":"thr_demo","seq":4,"eventId":"evt_demo4","occurredAt":"2026-08-16T00:00:04Z","itemId":"item_msg","delta":"已读取"}}
```

notification 没有 `id`、`result` 或 `error`，不产生 response；未知的普通扩展字段仍可保留。
Thread 事件必须包含 `serverInstanceId`、`threadId`、`seq`、`eventId` 和 RFC 3339
`occurredAt`。`seq` 从 1 开始，在一个 `serverInstanceId + threadId` 内严格递增；重启
会生成新的 `serverInstanceId`。Rust 发现 seq 缺口、重复或旧实例事件时停止直接归并，
重新读取 snapshot。

## 3. 初始化与版本

生命周期固定为：

```text
Rust -> initialize(request)
Java -> initialize(response)
Rust -> initialized(notification {readyToken})
Java -> runtime/statusChanged(ready {readyToken})
... full-duplex requests/events ...
Rust -> shutdown(request)
Java -> shutdown(response)
双方清理队列、pending、审批、Tool/MCP 子进程和锁
```

`initialize.params` 至少包含：

| 字段 | 约束 |
| --- | --- |
| `protocolMajor` | 首发必须是 `1`；不同 major 拒绝启动 |
| `protocolMinor` | 非负整数；server 选择双方可兼容的 minor |
| `minimumCompatibleMinor` | client 能理解的最低 minor |
| `clientVersion` | UI/host 版本字符串 |
| `capabilities` | 方法、事件、Item、访问模式和 MCP Tools 能力集合 |
| `limits` | frame、队列、pending、delta、inline 输出、日志和 deadline 上限 |

`initialize.result` 返回 `protocolMajor/protocolMinor/serverVersion/serverInstanceId`、
AgentScope/Java/runtime/native-image/os/arch 信息、最终能力集合和最终 limits。
major 不等于 1 或双方没有交集时返回 `PROTOCOL_VERSION_UNSUPPORTED`，随后关闭连接，
不得进入半初始化状态。minor 可以向后兼容：未知字段和未知非关键 capability 可以忽略，
但只有在 `minimumCompatibleMinor` 满足时才可继续。`initialized` 未收到前，除
`version`、`capabilities/read`、`health/read` 和 `shutdown` 外的 client request 返回
`NOT_INITIALIZED`。

`version` 返回构建版本、protocol、AgentScope、Solon、Java、Native Image、OS/arch；
`capabilities/read` 返回已协商能力与明确的 `unsupported` 条目。首发明确不提供
OpenAI Responses、ACP、WebSocket、OAuth、MCP Resources/Prompts/Sampling、插件热加载；
请求这些功能必须得到 `CAPABILITY_UNSUPPORTED` 或等价稳定错误，不得静默降级成另一种语义。

### 一次性 readiness challenge

`initialized` 是 Rust/Tauri 对当前 stdio pipe generation 的一次性 challenge，不是空
通知。Rust 必须使用系统 CSPRNG 生成一个新的 128-bit `readyToken`，以 32 个十六进制
字符传入 `initialized.params.readyToken`。它用于证明通知和后续 ready 事件属于同一代
管道，因此虽然不是 secret，仍禁止写入日志、诊断、trace、崩溃报告或持久化状态。

Java 只有在成功解析并接受这一条 `initialized` 后，才能发送
`runtime/statusChanged`，且 `status` 必须为 `ready`、`params.readyToken` 必须与
challenge 按字节原样相等。schema 只能校验 presence 和格式；相等性、顺序和一次性由
Rust/Java runtime invariant 校验。缺失、格式错误、错误、旧 generation、重复
`initialized` 或重复 `ready` 都必须以 `HANDSHAKE_FAILED` 终止本次握手，禁止进入
`ready`。

`starting`、`degraded`、`shutting_down`、`stopped` 和 `crashed` 等非 `ready` 状态不
携带 `readyToken`；schema 会拒绝带 token 的非 ready 状态。每次 sidecar 重启或
generation 切换都必须生成不同 token；旧 token 不能恢复新 generation 的 ready 状态。

整帧脱敏边界只有两个精确例外：当前 `initialized.params.readyToken`，以及与当前
challenge 匹配的 `runtime/statusChanged(ready).params.readyToken`。握手序列中任何其他
位置（包括 error response 外层 meta、runtime notice message/extensions、diagnostics、
result、provider/tool failure payload）出现 `readyToken` 键，或把当前/历史 challenge
原值作为 JSON object key/value，都必须拒绝；不允许通过未知扩展字段绕过。

## 4. 领域 ID 与权威状态

| ID | 创建方 | 作用域 |
| --- | --- | --- |
| `ws_...` | Java | Workspace 记录，不删除用户目录 |
| `thr_...` | Java | Thread 与事件序列的隔离键 |
| `turn_...` | Java | 一次输入到终态的运行单元 |
| `item_...` | Java | 可更新的时间线对象 |
| `evt_...` | Java | 事件唯一标识 |
| `appr_...` | Java | 一次审批，exactly-once 决策键 |
| `profile_...`/`skill_...`/`mcp_...` | Java | 不可变能力 revision |
| `srv_...` | Java | 一次 sidecar 实例；重启后必须变化 |

Java/SQLite 是 Workspace、Thread、Turn、Item、Approval、AgentScope state、Profile、
Skill 和 MCP revision 的唯一持久权威。Rust 只持有桌面布局、窗口、
用户 PTY、Preview、Secret 和 sidecar 生命周期状态。React 只缓存 snapshot 与增量，
不能把自己的 reducer 当成事实源。

## 5. Thread、Turn、Item 与 snapshot/live

### Turn

`turn/start` 只返回 `accepted`/`queued` 和 `turnId`。同一 Thread 同时最多一个 active
Turn；不同 Thread 可以并行。active Turn 收到新输入时由客户端排队到后续 Turn，v1 不
提供 steer 或 follow-up。

推荐状态：

```text
queued -> running -> waiting_approval -> running
running -> completed | failed | interrupting
interrupting -> interrupted | aborted_by_runtime
任何运行态 --sidecar 崩溃--> aborted_by_runtime
```

`turn/cancel` response 只是 ACK；只有 `turn/completed` 的 `terminalStatus` 为
`interrupted`、`failed` 或 `aborted_by_runtime` 才是终止事实。
取消必须同时处理 AgentScope interrupt、Reactor subscription、Java Tool operation 和
完整子进程树；只停止模型流不算取消成功。

### Item

首发 Item kind 是 `user_message`、`agent_message`、`commentary`、`tool_call`、`command`、
`file_change`、`approval`。AgentScope 内部的 Plan、Subagent 和 compaction 信号均映射
为普通 commentary。每个 Item 遵循 `started -> delta/update ->
completed/failed/cancelled`；`item/completed` 是完整可展示快照，delta 可以合并、丢弃或
短暂缓存，但不能替代最终快照。隐藏 chain-of-thought 不属于可发送的 Item 内容。

### Snapshot/live 无缺口

`thread/read` 返回一个只读 snapshot 和 `snapshotSeq`；可选 `afterSeq` 仅用于回放已持久
事件。`thread/subscribe` 在同一 connection 上登记订阅和 `fromSeq`，随后接收 live
notification。Rust 必须先创建本地 sink/缓冲，再发 snapshot request：

```text
注册 sink + bounded buffer
  -> thread/read(snapshot, snapshotSeq)
  -> 丢弃 seq <= snapshotSeq 的重复事件
  -> 按 seq 排空 seq > snapshotSeq 的缓冲
  -> 切到 live
```

关键事件在 SQLite 事务中分配 seq 并提交后才能进入 outbound queue。若提交后、发送前
崩溃，重启实例通过 snapshot 找回。队列满时不能丢弃或改写关键事件为成功；必须发
`runtime/overload`/`RESYNC_REQUIRED` 语义，客户端重新 snapshot。reducer 应按 `eventId`
和 `seq` 幂等。

## 6. Approval、Secret 与 External Tool

### Approval

Approval 唯一决定通道是 Java -> Rust 的 `approval/request` JSON-RPC request 和 Rust ->
Java 的标准 response。Java 先在同一个 SQLite 事务保存 Approval 与
`approval/requested` 事件，再发 request；Rust/React 将二者合并为一张卡片。response
只表示 `allow_once`、`allow_session` 或 `deny` 等决定已经记录，不代表 Tool 成功；Java
随后发送 `approval/resolved`，最终结果仍由 Tool Item 给出。

同一 `approvalId` 只有一个有效决定。超时、断开、未知 id、重复 response 和迟到 response
全部 fail-closed，使用 `APPROVAL_EXPIRED`、`DUPLICATE_RESPONSE`、`UNKNOWN_REQUEST_ID`
或 `LATE_RESPONSE` 等稳定错误/诊断，不得再次执行 Tool。approval request 必须包括
规范化动作、风险、当前 accessMode、关联 Thread/Turn/Item 和到期时间。

### Secret

`secret/resolve` 同样是 Java -> Rust request，但它是敏感 frame：不进入 React、事件、
trace、stderr、崩溃诊断、error message、argv 或普通环境变量。Rust 只接受当前
`serverInstanceId`、已激活 Profile/MCP revision、用途和 opaque `credentialRef` 均匹配的
请求，从 OS credential store 取值后通过同一匿名 pipe 返回。Java 只在 Model/MCP 调用期间
持有；OAuth 不属于首发能力。

### External Tool

`externalTool/request` 是 Java -> Rust 的受控宿主能力请求，和 Approval 一样使用标准
response。它不是 Java 把 Agent Shell 委托给 Rust；coding Agent 的文件、Patch、Shell
仍由 Java Tool runtime 和 Java-owned Tool Worker 执行。External Tool 只用于已明确纳入
能力协商的桌面桥接（例如 Secret 或未来平台能力），未知 Tool 必须拒绝。

## 7. Skills 与 MCP

- `skill/import` 只导入 built-in、user 或 workspace 本地目录/归档，校验 `SKILL.md`、编码、大小、来源和 hash；激活以不可变 revision 原子切换，坏 revision 保留 last-good。
- Skill 脚本不会因导入而执行；后续执行必须重新进入 Java permission/approval/sandbox Tool 链路。
- MCP 首发仅 `tools/list` 与 `tools/call`，传输仅 stdio 与 Streamable HTTP；Server 进程受限于生命周期、输出、timeout、Secret 和 sandbox policy。
- MCP OAuth、Resources、Prompts、Sampling、Roots、Elicitation、Apps 和插件市场不属于 v1；对应 capability 显示 unsupported，不能把认证 token 放进 URL。
- v1 输入仅接受有界文本；图片和二进制内容能力后置到独立版本。

## 8. 限额与背压基线

初始化双方分别宣告 limits，server 返回取双方可接受范围内的最终值。首发默认值为：

| 资源 | 默认上限 |
| --- | ---: |
| 单 frame JSON bytes | 4,194,304 |
| inbound queue | 256 frames |
| outbound queue | 1,024 frames |
| in-flight requests | 64 |
| pending server requests | 64 |
| 单 Item delta | 65,536 bytes |
| inline Tool output | 1,048,576 bytes |
| 诊断/日志缓存 | 1,048,576 bytes |
| 普通 request deadline | 120,000 ms |
| Approval deadline | 300,000 ms |

所有队列、pending、delta、Tool/MCP 输出、诊断日志和重试
必须有界。超过上限返回 `FRAME_TOO_LARGE`、`QUEUE_FULL`、`PAYLOAD_TOO_LARGE` 或
`BUDGET_EXCEEDED`，并说明 `retryable`；不能无限排队、无限重试或把大数据内联到 JSONL。

## 9. 错误、关闭与恢复

错误使用 JSON-RPC error object：`code` 是标准保留区间中的稳定整数，`data.jaCode` 是
机器消费的稳定大写码，`data.retryable` 表示调用方是否可安全重试，可带脱敏
`diagnosticId`、字段名和 `retryAfterMs`。不返回 SQL、绝对路径、token、stack trace、
完整 Prompt、源码或 Provider 原始 payload。

协议级错误（invalid frame、frame too large、unknown id、duplicate id、deadline、version）
不能伪装成 Tool failed。具有潜在副作用的 request 必须通过 `clientRequestKey`、
`approvalId` 或其他幂等键区分“未开始、已完成、结果未知”，未知结果不能自动重放。

正常关闭：Rust 发送 `shutdown`，Java 停止接受新 mutation、等待有界 deadline、收口
Tool/MCP/审批/DB writer 并返回 response，随后发 stopped 状态并 EOF。Rust 到期后
终止完整 sidecar 进程树。意外崩溃只允许有限指数退避重启；版本不兼容、数据目录锁、
迁移失败、签名/配置错误不得 crash-loop。未产生 terminal event 的 Turn 在新实例中标为
`aborted_by_runtime`，不承诺从模型或 Shell 中点继续。

## 10. 未实现能力的稳定边界

v1 不为 ACP、WebSocket/daemon、多客户端、OpenAI Responses、远程 OAuth、MCP 非 Tools
能力、动态 Java/JS 插件或 IDE 接入预留“空壳”。它们通过 `capabilities/read` 显示不可用，
调用时返回 `CAPABILITY_UNSUPPORTED`（OAuth 返回 `MCP_UNSUPPORTED_AUTH`）。新增能力必须
走 minor 演进或新 major，不能复用现有字段改变语义。
