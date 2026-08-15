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
cargo +1.88.0 metadata --manifest-path spikes/sandbox/macos/Cargo.toml --locked --no-deps
cargo +1.88.0 fmt --manifest-path spikes/sandbox/macos/Cargo.toml -- --check
cargo +1.88.0 check --manifest-path spikes/sandbox/macos/Cargo.toml --target x86_64-apple-darwin --locked
cargo +1.88.0 check --manifest-path spikes/sandbox/macos/Cargo.toml --target aarch64-apple-darwin --locked
cargo +1.88.0 clippy --manifest-path spikes/sandbox/macos/Cargo.toml --all-targets --locked -- -D warnings
cargo +1.88.0 test --manifest-path spikes/sandbox/macos/Cargo.toml --all-targets --locked
cargo +1.88.0 run --manifest-path spikes/sandbox/macos/Cargo.toml --bin ja-sandbox-probe --locked
```

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
