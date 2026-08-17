<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Invalid fixture 语义

- `envelopes.jsonl` 四行分别覆盖缺失 `jsonrpc`、response 同时有 result/error、错误的
  request-id namespace、显式 null params。
- `duplicate-late-limit.jsonl` 第一行模拟 deadline 后的迟到 response，第二行模拟同一
  id 的重复 response；第三行的 limits 全部低于协议下限，必须被 Schema/运行时拒绝。
- `mcp.jsonl` 覆盖 stdio 命令字符串/裸 credential、错误 transport/auth、HTTP userinfo 和
  credential query、混合大小写 credential key/value、percent-encoded query、unsupported protocol
  version、secret literal、credential shorthand 冲突、stdio header auth 与 args/map 上限；每个
  wrapper 的 `frame` 都必须被根 Schema 拒绝。

这些行都必须可以 JSON parse；前两行 duplicate/late 需要结合 pending/deadline 状态机
判定，不应仅凭 JSON Schema 宣称非法。测试不得因为接收到 late/duplicate response 而
恢复审批或执行副作用。
