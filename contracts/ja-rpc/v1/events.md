<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` 事件清单

事件是 JSON-RPC notification，不能带 `id`，也不等待 response。除 runtime 事件外，
每个事件的 `params` 都带 `serverInstanceId`、`threadId`、`seq`、`eventId` 和
`occurredAt`。Java 在 SQLite 事务内为权威事件分配 seq 并提交后，才把事件放入有界
outbound queue。`eventId` 去重和 `(serverInstanceId, threadId, seq)` 顺序检查都由
Rust/React 执行；缺口必须走 snapshot/resync，不能猜测补齐。

## Runtime 事件

| method | params 核心字段 | 终态/恢复语义 |
| --- | --- | --- |
| `runtime/statusChanged` | `serverInstanceId`、`eventId`、`occurredAt`、`status` (`starting/ready/degraded/shutting_down/stopped/crashed`)、`readyToken`（仅 `status=ready` 条件必填；非 ready 禁止）、`reason?`、`health?` | sidecar 生命周期；ready 必须带当前 generation 的 challenge；不带 Thread seq |
| `runtime/notice` | runtime 基础字段、稳定 `code`、脱敏 `message`、`threadId?`、`turnId?` | 说明恢复、重连、resync 或 enforcement，不改变事实状态 |
| `runtime/overload` | runtime 基础字段、`queue`、`retryable`、`retryAfterMs?` | 说明 inbound/outbound/pending/tool_output 超限；关键事实不能静默丢弃 |

`runtime/statusChanged(ready)` 只能在 `initialize`/`initialized` 完成且数据库、能力和
limits 已确定后发送，并且必须原样回显 `initialized.params.readyToken`。schema 只负责
校验 token 的 32 位 hex 格式；Rust/Java 必须额外校验 token 相等、generation 未切换、
没有重复 ready，并在失败时用 `HANDSHAKE_FAILED` 禁止晋级。challenge 不得记录到日志。
`crashed` 只表示本实例异常退出；新实例用新的 serverInstanceId 和新的 challenge，
并通过 snapshot 恢复已提交事实。

所有 `error` response、`runtime/notice`、诊断和 provider/tool 失败详情共享同一脱敏
边界：递归禁止 `readyToken` 键，也禁止把当前或历史 generation 的 challenge 原值作为
任意 JSON object key 或 value；不得通过 `details`、数组或嵌套 provider payload 绕过。

## Thread 与 Turn

| method | params 核心字段 | 说明 |
| --- | --- | --- |
| `thread/changed` | thread event 基础字段、`thread`、`change` (`created/updated/archived/deleted`) | Thread 元数据变化；最后 `lastSeq` 必须与事件序列一致 |
| `turn/started` | thread event 基础字段、`turn` | Turn 进入 running；同一 Thread 串行，不同 Thread 可并行 |
| `turn/waiting` | thread event 基础字段、`turn` | 仅表示 `waiting_approval`，原因放在 Turn/metadata |
| `turn/completed` | thread event 基础字段、`turn`、`terminalStatus` | 唯一 Turn terminal event；status 为 completed/interrupted/failed/aborted_by_runtime |

`turn/completed` 必须 exactly-once。`turn/cancel` 的 ACK、模型流结束、Java 进程退出
都不能替代它。终态发送前，AgentScope、Tool operation 和数据库写入必须完成；异常以
`aborted_by_runtime` 或 `failed` 结束。

## Item 时间线

| method | params 核心字段 | 说明 |
| --- | --- | --- |
| `item/started` | thread event 基础字段、完整 `item` | 创建稳定 `itemId`；重复 eventId 幂等 |
| `item/delta` | thread event 基础字段、`itemId`、`delta`、`deltaBytes?` | 短期流式增量；每条受 `maxItemDeltaBytes` 限制，可合并 |
| `item/updated` | thread event 基础字段、完整 `item` | Tool/progress 等可变 Item 更新 |
| `item/completed` | thread event 基础字段、完整 `item` | 最终可展示快照，必须覆盖完整结果 |

首发 `item.kind` 为：`user_message`、`agent_message`、`commentary`、`tool_call`、
`command`、`file_change`、`approval`。AgentScope 的 Plan、Subagent 和 compaction
信号映射为普通 `commentary`，隐藏 chain-of-thought 不得透传。大输出只使用有界的
inline 内容和截断标记，不新增句柄或产品级输出对象。

## Approval 与 External Tool 领域事件

| method | params 核心字段 | 说明 |
| --- | --- | --- |
| `approval/requested` | thread event 基础字段、`approval`（approvalId、action、risk、expiresAt） | Java 已在同一事务保存审批后发送；仅用于可恢复 UI 投影 |
| `approval/resolved` | thread event 基础字段、approvalId、decision、resolvedAt | 决定已经记录；不代表 Tool 成功 |
| `externalTool/requested` | thread event 基础字段、externalRequestId、toolName、thread/turn/item 可选 | 与 Java→Rust `externalTool/request` request 对应的可恢复投影 |

Approval 领域事件不是第二个应答协议。Rust 必须以 `approvalId` 合并 request 与
`approval/requested`，并只通过 `approval/request` 的标准 response 回传决定。

## Snapshot 与 live 事件顺序

1. Rust 为 Thread 注册 sink 与 bounded buffer，再发送 `thread/read`。
2. Java 在单一只读事务返回 `snapshot`、`snapshotSeq` 和当前 serverInstanceId。
3. Rust 丢弃 buffer 中 `seq <= snapshotSeq` 的重复事件。
4. Rust 按 seq 连续排空 `seq > snapshotSeq`，出现缺口就重新 snapshot。
5. 排空完成后切换 live；后续 reducer 按 eventId/seq 幂等。

snapshot 不被事件替代；事件也不保证包含完整历史。UI reload、sidecar restart、慢消费
或超限恢复都必须能以 snapshot + live 重建权威时间线。订阅取消不改变 Java 状态，
且 listener、channel、后台任务和子进程必须清理。
