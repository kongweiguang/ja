<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` 稳定错误

每个 error response 都遵循：

```json
{
  "jsonrpc":"2.0",
  "id":"c:req-1",
  "error":{
    "code":-32021,
    "message":"请求超过有界队列",
    "data":{"jaCode":"QUEUE_FULL","retryable":true,"retryAfterMs":250}
  }
}
```

`code` 是稳定 JSON-RPC server error code，`data.jaCode` 是稳定机器码，
`data.retryable` 是唯一重试提示；`diagnosticId` 只能引用脱敏日志。错误不得包含
SQL、绝对路径、token、secret、stack trace、完整 Prompt、源码或 Provider 原始响应。
脱敏边界递归适用于 `error`、`data`、`details`、数组和嵌套 provider/tool payload：禁止
出现 `readyToken` 键，也禁止把当前或历史 generation 的 challenge 原值作为任意 JSON
object key 或 value。`details`
只放可公开的字段名、限制值、版本或状态摘要。新增错误只追加，不复用旧码改变含义。

## 协议与队列

| code | `jaCode` | retryable | 触发与恢复 |
| ---: | --- | :---: | --- |
| -32001 | `INVALID_FRAME` | 否 | JSON 不是 object、UTF-8/LF/frame 结构不合法；关闭连接 |
| -32002 | `FRAME_TOO_LARGE` | 否 | 超过协商 `maxFrameBytes`；按 inline 上限截断或缩小请求 |
| -32003 | `PROTOCOL_VERSION_UNSUPPORTED` | 否 | major 不兼容或 minor 无交集；停止启动 |
| -32004 | `NOT_INITIALIZED` | 否 | `initialized` 前调用业务方法 |
| -32005 | `ALREADY_INITIALIZED` | 否 | 同一 sidecar 重复 initialize |
| -32006 | `METHOD_NOT_FOUND` | 否 | 未声明方法；先读取 capabilities |
| -32007 | `INVALID_PARAMS` | 否 | 缺失、null、类型、范围或关联 ID 不合法 |
| -32008 | `QUEUE_FULL` | 是 | inbound/outbound queue 到达上限；按 retryAfterMs 重试只读/幂等请求 |
| -32009 | `PENDING_LIMIT` | 是 | pending request 超限；等待现有请求收口 |
| -32010 | `DUPLICATE_REQUEST` | 否 | request id 或幂等键重复但原结果未知；不得自动重放副作用 |
| -32011 | `UNKNOWN_REQUEST_ID` | 否 | response 没有对应 pending request；忽略或关闭违规通道 |
| -32012 | `DUPLICATE_RESPONSE` | 否 | 已消费的 response 再次到达；不得再次恢复 Tool |
| -32013 | `LATE_RESPONSE` | 否 | deadline/取消/断开后到达；fail-closed |
| -32014 | `REQUEST_DEADLINE_EXCEEDED` | 是 | 普通 request deadline 到期；仅幂等查询可安全重试 |
| -32015 | `PAYLOAD_TOO_LARGE` | 否 | delta、inline Tool output、schema 或集合超限 |
| -32016 | `RESYNC_REQUIRED` | 是 | seq 缺口、事件丢失或订阅过载；重新读取 snapshot |
| -32017 | `HANDSHAKE_FAILED` | 否 | `initialized` challenge 缺失、格式错误、错误、旧代或重复；禁止进入 ready，关闭本代连接 |

## 运行时、数据与并发

| code | `jaCode` | retryable | 触发与恢复 |
| ---: | --- | :---: | --- |
| -32020 | `SHUTTING_DOWN` | 是 | 关闭阶段拒绝新请求；等待新实例 ready |
| -32021 | `DATA_DIR_IN_USE` | 否 | 另一实例持有 Java DB lock；聚焦已有实例 |
| -32023 | `MIGRATION_FAILED` | 否 | DB migration 事务失败；保留旧 DB 和备份，进入恢复页 |
| -32024 | `SCHEMA_TOO_NEW` | 否 | 数据库由更新版本创建；升级应用，不降级覆盖 |
| -32025 | `WORKSPACE_NOT_FOUND` | 否 | JA 记录不存在；重新 open |
| -32026 | `WORKSPACE_UNTRUSTED` | 否 | 未获得 workspace trust；仅可执行受限读取 |
| -32028 | `CONFLICT` | 是 | profile revision、expected hash 或 workspace 写入冲突；重新读取 |
| -32029 | `THREAD_NOT_FOUND` | 否 | Thread 不存在或已 purge |
| -32030 | `THREAD_BUSY` | 是 | 同一 Thread 已有 active Turn；使用 queue/cancel |
| -32031 | `THREAD_READ_ONLY` | 否 | 归档或 Read-only 上下文拒绝写入 |
| -32032 | `TURN_NOT_FOUND` | 否 | Turn ID 不存在 |
| -32033 | `TURN_NOT_ACTIVE` | 否 | cancel 目标不是 active Turn |
| -32034 | `INVALID_STATE` | 否 | 状态转换违反状态机 |
| -32035 | `CANCELLED` | 否 | 操作已按用户或 shutdown 取消；不代表副作用未知可重放 |
| -32036 | `BUDGET_EXCEEDED` | 否 | 时间、Token、Tool 或重试预算到达上限 |

## Permission、Tool 与 Sandbox

| code | `jaCode` | retryable | 触发与恢复 |
| ---: | --- | :---: | --- |
| -32040 | `APPROVAL_NOT_FOUND` | 否 | approvalId 不存在或不属于当前 instance |
| -32041 | `APPROVAL_EXPIRED` | 否 | 到期/断开后拒绝；不执行原 Tool |
| -32042 | `APPROVAL_ALREADY_RESOLVED` | 否 | 已有唯一决定；UI 刷新可读取 resolved 事件 |
| -32043 | `TOOL_DENIED` | 否 | Permission policy 或用户决定拒绝 |
| -32044 | `TOOL_FAILED` | 否 | Tool 已执行但失败；详情脱敏且不得伪装为协议错误 |
| -32046 | `PROCESS_TIMEOUT` | 是 | Tool/Worker 超时并已尝试清理进程树 |
| -32047 | `PROCESS_OUTPUT_LIMIT` | 否 | stdout/stderr 超过上限；剩余内容丢弃并标记截断 |
| -32048 | `EXTERNAL_TOOL_UNSUPPORTED` | 否 | 未协商的桌面桥接或错误方向 |

## Secret、Model、Skill 与 MCP

| code | `jaCode` | retryable | 触发与恢复 |
| ---: | --- | :---: | --- |
| -32050 | `SECRET_NOT_FOUND` | 否 | credentialRef 不存在；用户在设置中重新配置 |
| -32051 | `SECRET_ACCESS_DENIED` | 否 | revision/purpose/instance 不匹配；不猜测其他 ref |
| -32052 | `MODEL_UNSUPPORTED` | 否 | provider/protocol 或能力（如 vision）未探测支持 |
| -32053 | `MODEL_UNAVAILABLE` | 是 | Provider 暂时不可用；遵守 retryAfterMs 和用户预算 |
| -32054 | `SKILL_INVALID` | 否 | SKILL.md、编码、归档路径/hash/大小校验失败 |
| -32055 | `SKILL_UNAVAILABLE` | 是 | last-good revision 不可用或加载失败；显示 degraded |
| -32056 | `MCP_UNSUPPORTED_AUTH` | 否 | 首发不支持 OAuth 等远程认证；不能把 token 填 URL |
| -32057 | `MCP_SERVER_UNAVAILABLE` | 是 | MCP 进程/HTTP server 不健康；只撤销该 server Tool revision |
| -32058 | `MCP_PROTOCOL_UNSUPPORTED` | 否 | MCP version/transport/features 不在协商集合 |
| -32059 | `MCP_TOOL_NOT_FOUND` | 否 | Tool registry revision 不含该 namespaced tool |
| -32060 | `MCP_TOOL_FAILED` | 否 | Tool call 已执行但失败；不得无条件自动重放副作用 |

## 未实现能力与内部错误

| code | `jaCode` | retryable | 触发与恢复 |
| ---: | --- | :---: | --- |
| -32070 | `CAPABILITY_UNSUPPORTED` | 否 | ACP、Responses、OAuth、插件或 MCP 非 Tools 等未实现能力 |
| -32071 | `AUTH_UNSUPPORTED` | 否 | 需要账号/远程 OAuth 的能力；v1 无账号体系 |
| -32080 | `INTERNAL_ERROR` | 否 | 未分类内部错误；仅返回 diagnosticId，不能泄漏 cause |
| -32081 | `SIDE_CAR_CRASHED` | 否 | sidecar 已退出；由 host 启动有界恢复并标记未完成 Turn |
| -32082 | `SHUTDOWN_TIMEOUT` | 否 | 关闭 deadline 到期，host 需要终止完整进程树 |

标准 JSON-RPC 的 `-32600`（invalid request）、`-32601`（method not found）、`-32602`
（invalid params）和 `-32603`（internal error）可由兼容实现使用，但 JA 生产实现应
优先使用本表稳定 `jaCode`，并保持 message 脱敏。未知 `jaCode` 按不可重试内部错误处理，
未知 error field 可忽略。
