<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA `ja-rpc/v1` golden fixtures

这些 fixture 是 Rust、Java 和 TypeScript 共享的最小 wire 样例，不是产品数据库快照。
除 `invalid/` 与 `version/major-incompatible.json` 外，根 Schema
[`../ja-rpc/v1/schema/ja-rpc-v1.schema.json`](../ja-rpc/v1/schema/ja-rpc-v1.schema.json)
应能接受每一个 JSON document，以及每一个 JSONL 文件的每一行。

response envelope 本身没有 `method`，因此根 Schema 只验证通用
`result`/`error` 互斥结构；实现必须在 pending registry 保存 `request id -> method`，
收到成功 response 后按 [`../ja-rpc/v1/results.md`](../ja-rpc/v1/results.md) 选择对应
`$defs/*Result` validator。验证不能把一个 `turn/start` response 当成任意 method 的
成功结果。

| 目录 | 覆盖 |
| --- | --- |
| `valid/core.jsonl` | initialize/initialized/ready/version、Thread/Turn/Item、delta 与 terminal |
| `valid/handshake.jsonl` | starting → challenge → ready、stopped 后 generation 换 token |
| `valid/errors.jsonl` | `HANDSHAKE_FAILED` 稳定 code/jaCode/retryable 映射 |
| `approval/` | Java→Rust nested approval request 与标准 response；重复响应只用于语义测试 |
| `snapshot-live/` | snapshot response 的 `snapshotSeq` 与后续 live event |
| `secret/` | dummy credential resolve、内存注入 response、clear runtime notice；没有真实 secret |
| `mcp-skill/` | Skill import/enable、stdio/HTTP MCP save、auth、mcp/test、tools/read |
| `limits/` | 全部有限 queue/frame/delta/inline-output/deadline 能力 |
| `version/` | minor 兼容和 major 拒绝 |
| `invalid/` | missing/null、方向/id、duplicate/late 与超限语义样例 |

验证边界：仓库当前不带 Ajv/JSON Schema CLI；验证脚本通过 `uv` 临时提供
`jsonschema[format]`，不会假设系统 Python 已安装依赖。PowerShell 可直接运行：

```powershell
uv run --with 'jsonschema[format]' python contracts/golden/validate.py
```

脚本以 `Draft202012Validator` 加 `FormatChecker` 加载根 Schema，并按照 Results
mapping 用 request id 关联 response 进行二次 result 校验。PowerShell
静态检查仍必须覆盖所有 JSON/JSONL 的 parse、Schema `$ref` 的本地目标、作者/SPDX
头和 fixture 中的绝对路径/疑似 secret 扫描。`invalid/` 和 major-reject fixture
只要求 JSON parse；它们故意违反 Schema 或依赖运行时状态，不能被误报为 schema-valid。
`invalid/handshake-challenge.jsonl` 是例外：脚本会对每个 case 同时检查 `schemaValid`
期望，并对 schema 可表达的错误 token、重复、旧代和顺序场景运行 runtime invariant；
其中 ready 与 initialized 的字符串相等性不能由 JSON Schema 单独表达。
验证器还从 `ja-rpc/v1/errors.md` 建立唯一错误目录，校验 `code/jaCode/retryable` 对应关系，
并递归扫描握手序列的整帧：除 `initialized.params.readyToken` 与匹配的
`runtime/statusChanged(ready).params.readyToken` 外，任何 `readyToken` 键或当前/历史
challenge 原值作为 JSON object key/value 都拒绝，包括 error 外层 meta、runtime notice、
diagnostics/result/provider/tool failure 扩展。

当前 valid fixture 为 54 帧，invalid/major 为 47 帧；MCP 专用覆盖为 16 个 schema-invalid
case，握手专用覆盖为 6 个 valid 帧和
23 个 invalid cases，包含缺 token、格式错误、非 ready 携带 token、ready 缺失/错误、
重复 initialized、重复 ready、旧 generation token、ready-before-initialized、无 initialized
的非 ready 序列、initialized-only、无 ready 的非 ready 序列、notification envelope 字段越界、整帧
错误脱敏和错误码映射漂移。

所有路径均为 `src/...` 等 workspace-relative 示例；所有 credential/value 都是
`DUMMY_ONLY_NOT_A_SECRET` 级占位文本。生产日志、trace、UI 和 crash report 绝不能
复制 `secret/` response 的敏感字段。
