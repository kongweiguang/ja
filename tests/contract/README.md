<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# JA 三端协议验收 Gate

`run.py` 是本 Gate 的唯一入口。它先运行冻结的 reference validator，再把同一份
`contracts/golden` corpus 的绝对路径传给 Java、Rust 和 TypeScript 的实际协议入口。
每个 consumer 都必须逐帧调用本端 parser/state facade，并回传 corpus digest 和数量
marker；runner 会校验 exit code、超时、输出上限、digest 和数量，避免把“搜索到 fixture
字符串”误当作协议验收。

## 运行

Windows（JDK 25、Rust 1.88、Node 24、pnpm 10.33 和 uv）：

```powershell
python tests/contract/run.py
```

也可以使用 PowerShell 包装器：

```powershell
pwsh -NoProfile -File tests/contract/run.ps1
```

macOS CI 使用相同的 Python 入口：

```bash
python3 tests/contract/run.py
```

Vitest 使用仓库内的 `tests/contract/vitest.config.ts` 和 `--configLoader runner`；
通过 `JA_VITEST_FULL_SUITE` 在 85 个前端测试与 contract adapter 之间切换；transform 缓存目录
始终指向 OS temp，Vitest result cache 显式关闭，避免改写既有结果文件或在 `node_modules/.vite-temp` 生成新的仓库 artifact。runner 会把
`node_modules/.vite-temp` 纳入前后递归快照；仅清理这个已知目录留下的空目录，非空、新增或
变更内容仍会 fail-closed。快照对目录只记录存在性和类型、忽略目录 mtime；文件仍严格比较
size/mtime，并由 runner 内部 self-check 锁定目录 mtime-only、文件新增、改写和删除语义。

Java gate 要求 `java --version` 为 25；Rust gate 使用根 `src-tauri/Cargo.lock` 的
`cargo test --locked`（Windows 运行 57 个测试，macOS 跳过 4 个 Windows 进程树测试后
运行 53 个）；前端 gate 使用现有 `pnpm-lock.yaml`，不会安装新依赖。

Rust 阶段先用 `cargo test --no-run` 验证退出码和平台测试可执行文件，再独立运行
测试并校验 `test result` marker；contract adapter 还把临时 stdio child 接入真实 production
`SidecarSupervisor`/`Session`/`codec`，由 `Supervisor.start`、ready promotion、事件读取和
`shutdown` 重放 6 个 valid handshake、23 个 invalid case 以及 minor-compatible promotion；
定向诊断可通过临时 adapter 的 `JA_RUST_SUPERVISOR_REPLAY_ONLY=1` 只验证这组回放；构建阶段
不把运行时测试输出当成构建成功条件。

## 覆盖与边界

Gate 直接证明：

- reference validator 读到 46 个 valid frame、12 个 method-specific result、23 个
  invalid handshake case 和 31 个 invalid/major parse case；
- 三端 consumer 读取同一 corpus，并以同一 digest 回传；Java 的
  `HandshakeJsonlCodec`、Rust 的 production `decode_frame` 加
  `SidecarSupervisor`/`Session` 重放和 TypeScript 的 `parse*`/handshake facade 都实际处理
  frame，而不是只做文本扫描；TypeScript 还按 pending method identity 调用 12 个
  method-specific result schema；
- golden 中的未知 minor 扩展、major fixture、envelope one-of、`result:null`、UTF-8、
  LF/CRLF/partial/oversize、current/history token 整帧泄漏和错误目录会进入至少一个
  实际入口；Java/Rust/TS 既有定向测试命令另外锁定状态机、全双工 nested request、
  pending、cancel、ready-terminal race 和并发清理；
- 固定 seed 生成的 100 个合法、100 个非法 bounded property frame 会由三端读取，
  任何 crash、hang、越界输出或 secret/token 泄漏都会使 gate 失败。
- 每个被超时、输出超限或管道泄漏触发的 child 都共享一个 8 秒 monotonic cleanup deadline，
  Windows 额外限制最多 1024 个 descendant，并在 deadline 内重复 bounded rescan、按叶到根
  校验 creation stamp 后回收，再以重复空扫描确认无残留；process-tree self-test 会真实创建
  8 个 descendant。超出预算或扫描失败会 fail-closed，不会无限等待。
- Java 的 75 个测试、Rust 的平台化全量测试和前端的 85 个 Vitest 测试会先运行，随后
  runner 额外重跑 full-duplex/nested、pending、cancel 和 ready-terminal race 定向筛选；
  每个命令都必须在超时和输出上限内退出并出现预期 marker。

Gate 不证明：

- Java `App` 与 Tauri composition root 的真实跨进程 stdio wiring；该集成属于后续
  `INT-FOUNDATION`，当前 Java sidecar 还未接线；
- 真实模型、AgentScope tool call、MCP server、文件系统修改或生产 native-image
  产物；这些需要各自的集成/发布验收。

失败输出只包含 stage、case id/hash 和安全分类，不输出 fixture 内容、token、用户路径
或命令原始日志。临时 Java/Rust build source 与 property corpus 位于 OS temp，退出
时由 `TemporaryDirectory` 清理。
