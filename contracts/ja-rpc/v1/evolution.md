<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` 演进与兼容规则

`v1` 的兼容单位是 `protocolMajor/protocolMinor`，不是单独的 JSON Schema 文件名。
Schema、Markdown 和 golden fixtures 必须在同一变更中更新；实现升级前先运行旧 fixture，
再运行新 fixture。没有通过兼容验证的变更不能仅修改版本号掩盖。

## Minor 版本（兼容）

允许的 minor 变更：

- 增加可选 request/response/notification 字段；旧端必须忽略未知字段。
- 增加新的可选 method/event，并在 `capabilities` 中声明；旧端不得调用未声明方法。
- 增加新的非关键 capability、Item metadata 或 `error.data.details` 字段。
- 增加新的稳定错误码；旧端将未知 `jaCode` 按不可重试错误展示，并保留 diagnosticId。
- 提高实现内部能力，但不得降低协商后的 limits、sandbox enforcement 或错误安全语义。

minor 变更仍须满足 `minimumCompatibleMinor`。如果对端无法理解新增能力，可在
`capabilities/read` 中显示 unsupported，不能静默选择另一种行为。

## Major 版本（拒绝）

下列变化必须升 major，且 v1 对端必须在 initialize 阶段返回
`PROTOCOL_VERSION_UNSUPPORTED` 后关闭：

- 改变 JSON-RPC direction、request id 前缀、result/error 互斥或 LF-JSONL framing。
- 删除、重命名、改变必填字段类型或改变已发布 method/event 的状态语义。
- 改变 seq 的作用域/单调性、terminal event exactly-once 或 snapshot/live 无缺口流程。
- 将 Java→Rust Approval/Secret request 改成 notification、或允许 Rust 代理 Agent Tool。
- 放宽 frame、queue、pending、Tool output、artifact 或 deadline 上限，导致旧端无界接收。
- 把 OAuth、Responses、ACP、插件、MCP Resources/Prompts 等未实现能力伪装成 v1 能力。

## 字段语义

- 缺失字段与显式 `null` 不相同。只有 Schema 明确允许 `null` 的字段才可发送 null；
  可选字段缺失表示“使用默认/未知”，不能发送 null 代替空数组或空对象。
- 已知字段必须满足 v1 类型和上限；未知字段允许保留但不得被执行器当成权限、路径、
  Secret 或 Tool 参数。安全敏感字段以 compile-time method registry 决定，不能信任
  frame 自报 `sensitive`。
- IDs、seq、deadline、hash、size 和版本必须按 Schema 限制；JavaScript safe integer
  之外的未来计数器应改为字符串并升 major，不得截断。
- error message 面向用户但必须脱敏；客户端只根据 `jaCode`、`retryable` 和字段读取
  恢复策略，不匹配 message 文本。

## 方法/事件注册

新增 method/event 需要同时：

1. 添加 Schema definition 和 direction 限制。
2. 添加方法/事件表、状态转换和能力名称。
3. 添加 valid、invalid、unknown-field、missing/null 和 late/duplicate golden。
4. 为 Rust、Java、TypeScript caller 添加 round-trip/拒绝测试。
5. 更新实现版本的 `capabilities`，再按 minor 规则发布。

Approval、Secret、External Tool 的 server request 必须继续使用 `s:` ID；不能复用普通
client request 的 `c:` namespace。`approvalId` 的决策与 `eventId`/seq 去重规则永久稳定。

## 兼容测试矩阵

| 测试 | 旧端预期 | 新端预期 |
| --- | --- | --- |
| v1.0 frame ↔ v1.0 | 双向通过 | 双向通过 |
| v1.0 frame ↔ v1.minor+1，仅新增可选字段 | 忽略未知字段并保持状态 | 使用已知字段，明确隐藏新增能力 |
| v1.0 ↔ major+1 | initialize 拒绝并停止 | initialize 拒绝并停止 |
| duplicate/late response | 不重复恢复、不执行副作用 | 返回稳定诊断/关闭违规请求 |
| seq 缺口/旧 instance | resync snapshot | 保留已提交事实，不伪造连续序列 |
| unknown method/capability | `METHOD_NOT_FOUND`/`CAPABILITY_UNSUPPORTED` | 不静默 fallback |
| missing/null/unknown field | 缺失按默认，null 按 Schema，未知忽略 | 同样语义并保留安全边界 |

Golden fixture 是跨语言兼容的最小证据，不是完整产品状态快照。生产实现必须额外覆盖
并发、背压、取消、进程树清理、数据库恢复、Native Image、真实 Tauri runtime 和
Windows/macOS 安全边界。
