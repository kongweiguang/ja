<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Tauri sidecar 生命周期探针

该目录是 JA 生产 `src-tauri` 之外的隔离验证 crate。它验证 future Rust host
管理 Java/GraalVM native sidecar 时必须成立的边界：

- 显式 `Starting -> Ready -> Stopping -> Exited` 与 `Backoff/Incompatible` FSM；
- 绝对 executable、结构化参数、独立 cwd、清空后只注入 allowlist 环境变量；
- 单 writer stdin、stdout JSONL framing、持续 drain stderr、有界事件/写入队列；
- initialize/ready barrier、graceful deadline、强制完整进程树清理、有限指数退避；
- Windows 通过 `CREATE_SUSPENDED -> AssignProcessToJobObject -> ResumeThread` 建立
  `KILL_ON_JOB_CLOSE` ownership，再让 child 执行第一条用户指令；Unix/macOS 使用
  `process group`；
- `serde_json` 严格解析 JSON object，并按完整 JSON-RPC method/params/error 字段
  识别 ready 与 incompatible，普通 payload 不能伪造握手；
- cwd 必须是已存在的 canonical directory，并且不能位于 workspace root 或其子目录；
- 普通输出丢弃只产生可观察 overflow，控制事实（ready、退出、协议违规）不会被普通
  delta 挤掉；控制队列耗尽时 fail-closed 并终止 sidecar；
- supervisor 观察到 fatal control overflow 时会在返回诊断事件前切到 `Exited` 并
  同步回收完整进程树，后续写入拒绝；调用方无需额外记住 shutdown；
- `ProcessExited` 带 child generation；重启后迟到的旧退出事件只保留诊断，不能收口
  或终止新实例；
- native helper 的污染、半帧、非零退出、crash/restart 和孙进程清理；
- `fixtures/java/JaFixture.java` 通过真实 `java` executable 的最小 JVM 闭环。

## 聚焦验证

在仓库根目录下的 `spikes/tauri-sidecar` 目录执行：

```powershell
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

Windows 交付还应重复运行 `cargo test --all-targets --locked`，并确认 fixture
进程与孙进程均已消失；macOS 交叉检查使用 `cargo check --target
x86_64-apple-darwin --all-targets --locked` 和 `aarch64-apple-darwin`，这只能证明
目标代码可编译，不能替代 macOS 真机进程树验收。

Java fixture 集成测试会优先使用 `JAVA_HOME`，未配置时再按当前平台的
`PATH` 查找同一目录下的 `javac` 与 `java`。缺少可用 JDK 时仅跳过 JVM 场景，
Rust native helper 场景仍必须通过；交付报告不得把跳过写成通过。

## 平台证据边界

Windows Job Object 和真实 process-tree cleanup 在 Windows runner/本机验证；
macOS 只可在 macOS runner 运行 `cargo check/test` 后报告，Windows 结果不能替代
macOS 真机行为。该 spike 不启动 Tauri UI，也不修改 `agent/**`。
