<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# `ja-rpc/v1` 协议探针

本目录是隔离的并发/ framing 探针，不属于生产 Java、Rust 或 Tauri 模块。它直接读取
`../../contracts/golden/valid/core.jsonl`，因此不会复制第二套协议说明。

探针验证的生产不变量：

- LF-JSONL 必须完整以 LF 结束；半行、invalid UTF-8、非 JSON stdout 和非 object 都明确失败。
- `BufRead::fill_buf/consume` 逐行 framing 只消费当前 LF；连续两帧不会因为一次 OS read 被吞掉，
  无 LF 超过 `maxFrameBytes` 会立即失败。
- envelope 还检查 `jsonrpc == 2.0`，request/notification 与 response 的字段形态，response 必须
  exactly-one `result`/`error`。
- reader 永久读取，writer 由单一 owner 串行写出；`c:` 与 `s:` request namespace 不串线。
- server 等待 `s:approval-1` response 时仍处理 `c:version-1`，避免 Java/Rust 嵌套 request 死锁；
  stderr 由有界 tail reader 持续消费，避免日志 pipe 反向阻塞 child。
- pending registry 的 active capacity 在 terminal 后立即释放，duplicate/late 分类保存在有界
  tombstone；inbound/outbound queue 都有显式上限，queue full/pending limit 可观察。
- deadline、cancel、unknown、duplicate、late response 都 fail-closed；只有第一次 response 可以恢复副作用。
- snapshot/live reducer 用有界按 seq 排序 buffer，对 duplicate 幂等，对乱序和 seq gap 要求
  snapshot/resync，不猜测补齐。

## 验证命令

在仓库根目录执行：

```powershell
cargo test --manifest-path spikes/protocol/Cargo.toml --locked
```

通过定义是：库单测和真实 `probe-child` 子进程集成测试全部通过，子进程收到 shutdown
后退出；测试没有使用任意 sleep，只有最终 3 秒 watchdog 防挂。依赖版本在
`Cargo.toml` 中精确锁定，`Cargo.lock` 由 Cargo 生成。

本探针不覆盖真实 Java AgentScope runtime、Tauri handler、Native Image、OS sandbox、
SQLite recovery 或 macOS process-tree；这些由后续 `TEST-CONTRACT`、`INT-JAVA`、
`E2E-CROSS` 和 delivery 任务验收。
