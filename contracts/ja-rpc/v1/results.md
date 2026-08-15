<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` Result schemas

JSON-RPC response 没有 `method` 字段，因此根 Schema 的 response envelope 必须保持
通用：它只校验 `jsonrpc`、`id` 以及 `result`/`error` 二选一。实现端在发出 request
时把 `id -> method/deadline/revision` 登记到 pending registry；收到同 id 的成功 response
后，再按本表选择 `$defs` 下的 result schema 验证。未知、迟到、重复或未登记的 id 不得
被当成任意方法结果消费。

根 Schema 中的 result definitions 位于
[`schema/ja-rpc-v1.schema.json`](./schema/ja-rpc-v1.schema.json) 的 `#/$defs`。字段
允许未来 minor 增加未知字段，但表中必填字段和枚举不能在 v1 内改变。

## Runtime 与 Workspace

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `initialize` | `initializeResult` | `protocolMajor`、`protocolMinor`、`serverVersion`、`serverInstanceId`、`capabilities`、`limits` |
| `version` | `versionResult` | `protocolMajor`、`protocolMinor`、`serverVersion`、`serverInstanceId`、`runtime` |
| `capabilities/read` | `capabilitiesResult` | `capabilities`；可选 `unsupported[]` |
| `health/read` | `healthResult` | `status`、`checks` |
| `diagnostics/read` | `diagnosticsResult` | `status`；可选脱敏 `report`/`artifact` |
| `shutdown` | `shutdownResult` | `accepted`、`status`；可选 `deadlineMs` |
| `workspace/open` | `workspaceOpenResult` | `workspace` |
| `workspace/list` | `workspaceListResult` | `workspaces[]`；可选 `nextCursor` |
| `workspace/trust/set` | `workspaceTrustResult` | `workspaceId`、`trust` |
| `workspace/unregister` | `workspaceUnregisterResult` | `accepted`、`workspaceId`；可选 `removed` |

`workspace/open` 结果中的 `rootPath` 是 Java 重新 canonicalize 后的可信摘要；UI 不应
把它作为任意命令路径直接回传给 Agent Tool。`shutdown` 的 accepted 只表示进入关闭
流程，sidecar stopped 和 EOF 才是生命周期终态。

## Thread、订阅与 Turn

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `thread/create` | `threadCreateResult` | `thread` |
| `thread/list` | `threadListResult` | `threads[]`；可选 `nextCursor` |
| `thread/read` | `threadReadResult` | `serverInstanceId`、`thread`、`items[]`、`snapshotSeq`；可选 `events[]`、`nextSeq` |
| `thread/subscribe` | `threadSubscribeResult` | `accepted`、`subscriptionId`、`fromSeq` |
| `thread/unsubscribe` | `threadUnsubscribeResult` | `accepted`、`subscriptionId` |
| `thread/archive` | `threadMutationResult` | `accepted`、`threadId`、`status=archived` |
| `thread/delete` | `threadMutationResult` | `accepted`、`threadId`、`status=deleted` |
| `thread/purge` | `threadMutationResult` | `accepted`、`threadId`、`status=purged` |
| `turn/start` | `turnStartResult` | `accepted`、`turnId`、`queued`；可选 status |
| `turn/cancel` | `turnCancelResult` | `accepted`、`turnId`、`status=interrupting/interrupted/recovery_required` |
| `turn/steer` | `turnSteerResult` | `accepted`、`turnId`、`queued` |
| `turn/followUp` | `turnFollowUpResult` | `accepted`、`turnId`、`queued` |

`thread/read` 的 `snapshotSeq` 是同一只读事务里的截止序号；它与
`thread/subscribe.fromSeq` 一起形成 snapshot→live 无缺口流程。`turn/start`、
`turn/cancel`、steer 和 follow-up 的 response 都只是 ACK/排队结果；Turn terminal
事实只能由 `turn/completed` notification 表达。

## Profile 与 Model

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `profile/list` | `profileListResult` | `profiles[]`；可选 `activeProfileRevision` |
| `profile/read` | `profileReadResult` | `profile` |
| `profile/save` | `profileSaveResult` | `profile`；可选 `created` |
| `profile/activate` | `profileActivateResult` | `accepted`、`activeProfileRevision` |
| `model/probe` | `modelProbeResult` | `supported`、`status`、`capabilities` |
| `model/capabilities/read` | `modelCapabilitiesResult` | `models[]` |

Model result 的 `capabilities` 是实际 provider/protocol 探测结果，不是用户配置自报；
OpenAI Responses、OAuth 或未实现 provider 不得通过 result shape 伪装为可用。

## Skills

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `skill/list` | `skillListResult` | `skills[]`，每项含 revision/name/scope/enabled/status |
| `skill/import` | `skillImportResult` | `skillRevision`、`status`；可选 contentHash |
| `skill/enable` | `skillEnableResult` | `skillRevision`、`enabled`；可选 scope |
| `skill/reload` | `skillReloadResult` | `skillRevision`、`status` |
| `skill/health/read` | `skillHealthResult` | `skillRevision`、`status`；可选 `issues[]` |

Skill result 只描述不可变 revision 和健康状态；导入/enable/reload 的成功不代表脚本
已经运行。脚本仍必须在后续 Tool permission/approval/sandbox 链路中执行。

## MCP Tools

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `mcp/list` | `mcpListResult` | `servers[]`，每项含 mcpRevision/status |
| `mcp/save` | `mcpSaveResult` | `server`；可选 `created` |
| `mcp/delete` | `mcpDeleteResult` | `accepted`、`mcpRevision` |
| `mcp/test` | `mcpTestResult` | `mcpRevision`、`status`；可选协议/Tool 数 |
| `mcp/reload` | `mcpReloadResult` | `mcpRevision`、`status` |
| `mcp/tools/read` | `mcpToolsReadResult` | `mcpRevision`、`tools[]` |
| `mcp/toolPolicy/set` | `mcpToolPolicyResult` | `mcpRevision`、`toolName`、`policy` |

MCP result 只覆盖 Tools registry。Resources、Prompts、Sampling、OAuth 等未实现能力
不能通过扩展 result object 被隐式启用；MCP server 自报 read-only 也不能跳过 Tool policy。

## Attachment 与 Java→Rust request

| request method | result `$defs` | 必填核心字段 |
| --- | --- | --- |
| `attachment/import` | `attachmentImportResult` | `attachment`，含 opaque id、artifact、size/hash/media |
| `attachment/read` | `attachmentReadResult` | `attachment` |
| `attachment/delete` | `attachmentDeleteResult` | `accepted`、`attachmentId` |
| `approval/request` | `approvalResponseResult` | `decision`、`resolvedAt`；可选 scope |
| `secret/resolve` | `secretResolveResult` | `secretValue`；可选 expiresAt（敏感字段） |
| `externalTool/request` | `externalToolResponseResult` | `accepted`、`status`；可选 output/artifact |

`approval/request`、`secret/resolve` 和 `externalTool/request` 的 pending id 必须是
`s:`。Approval response 只确认用户决定，Secret response 只在 Java 内存中短暂使用，
External Tool response 不能被解释为 Java Agent Shell 的代理执行结果。

## Error 分支

如果 response 带 `error`，pending registry 按 request method 记录失败并使用
[`errors.md`](./errors.md) 的 `jaCode/retryable`，不再尝试 result validator。缺少 result
和 error、同时存在两者、id 未登记、id 已超时或重复的 response 都不是普通 method error，
必须进入协议状态机的 duplicate/late/unknown 分支。
