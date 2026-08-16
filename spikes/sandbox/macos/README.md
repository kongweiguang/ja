<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- @author kongweiguang -->

# JA macOS sandbox spike

这个探针验证 JA Tool Worker 的 macOS Seatbelt 候选，不是生产 Tauri
adapter。它通过 `/usr/bin/sandbox-exec` 加载 `deny default` profile，并让
wrapper 在 exec 前形成独立 process group；超时、取消、输出超限和 parent 正常
退出都会先收口整个 group，再在硬 deadline 内 drain 非阻塞 stdout/stderr pipe。

## 真实边界

- `sandbox-exec` 不存在、profile capability probe 被系统拒绝、profile 语法错误或
  任何 native worker 失败都会返回失败；没有 unsandboxed/path-only fallback。
- profile 只允许动态加载所需的系统只读路径、固定 resource worker、workspace
  读写与 worker 自身 exec；metadata 仅覆盖 loader、worker/workspace 的根组件和
  精确父目录。resource sibling、workspace executable、product DB、secret、`..`、
  symlink escape 均必须失败，且 smoke 单独验证 protected metadata 不可见。
- worker、workspace 和 profile parent 在准备阶段只 canonicalize 一次；profile 与
  实际 exec/current directory 共用同一组路径，避免 `/var` 与 `/private/var` alias drift。
- process-info 与 signal 仅允许 `same-sandbox` target，不开放 unrestricted process control。
- workspace 内 multiply-linked regular file 在 spawn 前 fail-closed；同文件系统的
  protected hardlink fixture 创建成功时必须拒绝启动且不生成 profile/worker report。
- parent 环境被清空后只注入 `HOME/USER/LOGNAME/SHELL/TERM/TMPDIR`，`PATH` 和
  parent marker/Provider Secret 不可见；profile 不授予 loopback 或 external
  network capability。
- smoke 在 host listener 存在时验证 loopback deny；external address 作为第二个
  deny case，但是否有网络由 runner 决定，因此不能单凭外部连接失败证明网络策略，
  必须同时保留 profile 的显式 `deny network*` 与 loopback native evidence。
- 每次 worker 前后检查 ACL、mode、content hash 和 xattr；profile 使用 `0600`、probe
  temp root 使用 `0700`，profile、Unicode/space temp tree、report、worker 和 child
  必须在 probe 结束后清理。
- clean workspace preflight 等待 report 或 direct child terminal；若 worker 提前退出，
  只输出 exit/stderr category，不输出 profile 路径、命令参数或原始错误内容。
- CI 或显式设置 `JA_SANDBOX_DIAGNOSTICS=1` 时，probe 会短暂订阅
  `com.apple.sandbox.reporting` 的 `log stream`，只聚合固定白名单中的
  operation/category/process 和计数；stdout/stderr 采用非阻塞读取，整体有 64 KiB
  字节、单行 8 KiB、256 事件、64 个 key；身份查询的 stdout/stderr 还分别受 1 KiB
  上限和同一绝对 deadline 约束，无换行 raw buffer 超限会立即丢弃。诊断读取错误只
  输出固定 `io` 类别；诊断不可用或被截断时只留下脱敏状态。每个 kill/reap
  phase 最多 3 秒，`cleanup_child` 最坏 6 秒；若仍失败，Drop 再执行一个最多 6 秒的
  SIGTERM/SIGKILL 双阶段和一个最多 3 秒的最终 SIGKILL/reap，单个 Drop 路径最坏 9
  秒（`finish` 已经失败时，调用方到 Drop 的总上界为 15 秒）。绝不调用无界
  `Child::wait`，仍未确认 direct reap 或 process-group `ESRCH` 就保留所有权并触发显式
  fail-safe abort，交给外层 watchdog 和精确 marker 清理接管。
  CI 下先在 spawn 前以 0600 准备 primary/fallback/emergency marker；spawn 后再将精确
  PID/PGID、owner、nonce、executable kind 与 start identity 原子替换激活。每个 marker
  打开时使用 no-follow/close-on-exec，并在写前后校验 regular-file、当前 uid、0600、单
  hardlink 和 inode identity。rename 后同步 parent directory；这只承诺已向 macOS 文件
  系统 API 请求 file/parent-directory durability，不承诺底层磁盘、runner 异常断电或
  外部存储的更强保证。workflow 以 20 秒短窗口校验文件名、全部身份字段和 stat 属性，
  并调用与探针相同的 `marker_cleanup` Rust 实现；它从内核 errno 严格区分 `ESRCH`
  （已消失）与 `EPERM`（权限异常），绝不把任意非零结果当作清理成功。只清理 exact
  group/direct PID，且只有 direct reap 与 group `ESRCH` 同时成立才删除三份 evidence。
  marker cleanup 的每个身份查询 stdout/stderr 共享 1 秒绝对 deadline，各自 4 KiB 上限；
  query helper 的 `Child` 所有权在 direct reap 被确认前不会释放，try_wait 错误或
  bounded reap 失败会继续 kill/reap，仍无法证明时显式 fail-closed abort。fixture
  launcher 的 pipe、输出、PID 解析和后续 `?` 传播也统一经过同一个有界 Child
  finalizer；二次 reap 仍失败时固定输出安全类别并 abort，不让 live Child 进入 Drop。
  fixture 的 descendant group 在每个早期失败返回前再执行两次有界 SIGKILL/ESRCH
  收口；residual、EPERM 或其他 probe 错误固定映射为 abort 类别，不能依赖尚未创建的
  marker 或 workflow glob 推迟清理。
  目录 entry
  读取错误会清空已收集的 signal target 并报告固定 `marker-entry-invalid`，不会静默
  忽略不完整扫描。group cleanup 最长 20 秒，workflow 外层还设置 2 分钟 timeout，不能
  把 timeout 当作已确认的 reap 或 `ESRCH`。
  当前 `EPERM` fixture 验证的是 errno 分类和 fail-closed 语义，不声称已构造真实
  signal 权限拒绝场景；真实权限行为仍由原生 runner 观察。
  marker 激活失败会保留可用的 fallback/emergency evidence，并让 job 失败；
  若所有安全文件通道均不可用，probe 仍先在进程内 bounded cleanup，无法确认时显式
  abort，外层不得声称已完成精确清理。残留、无效或 owner 不匹配证据也使 job 失败。
  无论诊断状态如何，均不放宽 profile、跳过任何安全门禁或输出路径、源码、密钥和原始
  统一日志。

## 进程树不变量

`process_group(0)` 在 child exec 前生效，所有普通 descendant 继承同一组。Host
stdout/stderr 是非阻塞 pipe，各自有 64 KiB 默认上限（且不能超过 1 MiB hard cap）；发现 timeout/cancel/overflow 或
direct parent 退出时先 `kill(-pgid, SIGKILL)`，再 drain 至 EOF。pipe 未在 2 秒 cleanup
deadline 内关闭时直接失败，不进行无界 join，覆盖：

- blocked parent + grandchild；
- parent 正常退出但 grandchild 继承 stdout/stderr pipe；
- grandchild 持续输出导致 bounded reader overflow；
- escaped `setsid` grandchild 持续输出；overflow 后 host 立即停止读取并关闭 pipe，不能
  让单次 cleanup 被无限写入拖住；
- 显式 cancel；
- descendant 尝试 `setsid()` 创建新 session/group。若 runner 允许其存活，probe 会
  清理该 PID 后失败，作为生产 blocker，不把普通 process group 结果冒充完整隔离。

## 本地验证边界

Windows/Linux 只能执行格式化、依赖解析和 macOS target 编译检查，不能代替原生
Seatbelt 运行。目标 runner 必须是 GitHub Actions 原生 `macos-26-intel`（x64）与
`macos-26`（arm64），且两个独立 job 都运行三轮集成测试和一轮 direct probe。

```bash
cargo +1.88.0 metadata --manifest-path spikes/sandbox/macos/Cargo.toml --locked --format-version 1 --no-deps
cargo +1.88.0 fmt --manifest-path spikes/sandbox/macos/Cargo.toml -- --check
cargo +1.88.0 check --manifest-path spikes/sandbox/macos/Cargo.toml --target x86_64-apple-darwin --locked
cargo +1.88.0 check --manifest-path spikes/sandbox/macos/Cargo.toml --target aarch64-apple-darwin --locked
cargo +1.88.0 clippy --manifest-path spikes/sandbox/macos/Cargo.toml --all-targets --locked -- -D warnings
cargo +1.88.0 test --manifest-path spikes/sandbox/macos/Cargo.toml --all-targets --locked
cargo +1.88.0 run --manifest-path spikes/sandbox/macos/Cargo.toml --bin ja-sandbox-probe --locked
```

CI 的 marker-cleanup fixture 由同一个 `marker_cleanup` Rust binary 执行 forged、
pending、descendant 与 `EPERM` 分类断言；Bash 只负责传递 runner 参数，不复制进程
解析或 signal 逻辑。因此脚本 fixture 的通过不能替代下方原生 Seatbelt probe。

其中最后两条只能在 macOS 原生环境运行；非 macOS 明确失败，不能改成 skip。

## 不能证明的边界与生产门槛

- 同一普通用户在 profile/path preflight 之后主动竞态创建 symlink/hardlink 的攻击
  不被这个 spike 完整覆盖；正式写回必须携带 expected hash/revision，写前再次校验
  inode、mode、xattr 和内容，并由固定签名 worker resource 执行。
- Seatbelt 不是 kernel/VBS 级恶意代码隔离，也不替代签名、公证、依赖供应链审计、
  Tauri capability 或 Java provider Secret 隔离。Java core 仍不应整体放进 workspace
  sandbox，只有 Java-owned Tool Worker 进入该边界。
- `/usr/bin/sandbox-exec` 在未来 macOS 版本消失或被限制时必须报告
  `SANDBOX_POLICY_UNAVAILABLE` 并停止 Tool Worker，不能自动切 Full Access。
