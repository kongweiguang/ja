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
- profile 只允许动态加载所需的系统只读路径和 system-only `file-map-executable`、固定 resource worker、workspace
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
  只输出 exit/stderr category，不输出 profile 路径、命令参数或原始错误内容；若发生
  启动失败，会额外运行 `/usr/bin/true` 的同 profile 形状 loader control，输出
  `loader-baseline=pass|exit|signal|...` 固定类别，但不会用 control 替代 worker 断言。
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
  marker 或 workflow glob 推迟清理。descendant fixture 读取同一次生产 cleanup
  report 时只映射为固定的 `fixture-descendant-cleanup-{residual,eperm,signal,query,
  identity,remove,scan,unsafe,unknown}` 类别；report 不可读则为
  `fixture-descendant-cleanup-report`，不会把具体 PID、路径或系统错误文本带入 CI。
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
  统一日志。CI 使用 `JA_SANDBOX_PRIVATE_ROOT` 在 runner temp 下创建独立 `0700`
  evidence parent；scope/marker/setsid escape 路径和 owner-private 证据目录先准备，但不会发布空
  scope；每次
  worker spawn 拿到 PID/PGID 后立即原子写入 provisional identity，再查询 start identity
  并原子升级。成功运行才由外层 gate 删除已确认清理的 scope；查询、升级或注销失败会
  保留 scope identity 或 `.scope.failure` 固定失败证据，并由同一个
  `marker_cleanup --residual-scan` Rust helper 读取。scope 父目录由 probe 原子创建为
  owner-only `0700`，Seatbelt worker 没有该目录的 allow；这保证 sandbox child 不能访问
  marker/scope 管理目录。process-table 清理先验证全部 scope 的 descriptor/path
  `st_dev/st_ino/nlink/mode/uid`，再把每份文件移入同一 private parent 的 quarantine。
  在任何 rename 前，owner-only manifest 会先用同目录 `O_EXCL` 临时 inode、`0600`
  和 no-follow 写入并 fsync 每份候选的完整 scope 内容（hex）、内容 digest 和 inode
  identity，再以原子 rename 发布；稳定 manifest 路径绝不暴露半写内容。clean report
  durable 后才逐份复核并删除 quarantine，最后再次校验 manifest 的 descriptor/path
  identity、同步目录并删除 manifest。manifest 删除后的目录 fsync 若失败，会用内存中
  的完整 image 和 identity 重新以同样的临时 inode 原子恢复；恢复不确定时保留上层
  failure evidence 并 abort。任一 rename、fsync、report、复核、unlink 或 manifest
  操作失败都会保留 quarantine/manifest（manifest 足以恢复和诊断原始受限 evidence），
  并继续尝试其余 entries，不把缺失唯一证据报告成 clean。每个 marker/process identity signal（group 或 direct）前都重新查询并严格匹配
  原 capture 的 PID、PGID、comm、start identity；PID reuse、identity mismatch 或
  reserved id 只产生固定 identity-lost failure，绝不发送 signal。
  capability probe、xattr、ACL、hash、`ps` 和 `log stream` 均通过同一个带进程组的
  bounded-child 入口创建；除该底层入口外，owned native code 不直接调用裸
  `Command::spawn/output/status`。每个 host helper 都有独立 stdout/stderr cap、共享
  absolute deadline、direct reap 和 group `ESRCH` 收口。
  helper 对每个 scope 运行有界、非阻塞的 `/bin/ps` 查询，严格解析受控的 PID、PGID、
  `lstart` start identity 和 comm；0600 scope 文件中的根路径只作辅助审计值，绝不由
  路径子串或命令行路径单独授权匹配。PID reuse、start/PGID/comm 不一致、
  malformed/Unicode identity、scope entry 或 ps 行解析不确定都会 fail-closed；同路径参数
  的无关进程不会被误报。stdout 与 stderr 各自累计 256 KiB hard cap，共享同一 2 秒绝对
  deadline，连续多轮输出也不能重置预算。仅输出
  `process-table-residual/query/output/scope/parse/identity` 等固定类别和计数，不复制
  Bash 进程名逻辑，也不向 artifact 写入 PID、路径或命令行。`require_seatbelt` 的
  `(allow default)` 仅用于探测 `/usr/bin/sandbox-exec` 能力，永不进入 worker profile；
  worker profile 仍保持 `(deny default)` 与精确 allowlist。
- scope evidence 由唯一的 `ScopedNativeChild` owner 注册；每个 direct child 成功
  reap 且原始 PGID 得到 `ESRCH` 后，才以原子重写、rename 和 parent-directory sync
  注销该 child。最后一个 child 注销时删除空 scope，下一次 spawn 会重新创建 owner-only
  scope。注册、重写、注销或恢复失败都会保留旧 identity 并使 gate 失败；`.scope.pending-*`
  半发布文件和 `.scope.failure` 都属于残留证据，不能由 residual scan 当作干净；所有
  PID/PGID/owner identity 的 `0/1/-1` 保留值都在解析和 signal 前拒绝。hardlink 异常与
  `/usr/bin/true` loader baseline 都经过同一注册入口，不存在绕过 scope 的 native spawn。

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
  在 report 后、host cleanup 前 capture 该 descendant 的 PID/PGID/comm/start identity，
  再按同一 identity-checked group/direct 路径清理并失败，作为生产 blocker，不把普通
  process group 结果冒充完整隔离。

setsid negative cases 在 spawn 前先以 `0700` private root 和原子创建的 `0600`
provisional escape evidence 记录 operation/parent identity 及 unknown descendant 状态；
setsid report 只接受完整的 `setsid-denied=true`，或恰好一次
`setsid-started=true` 加恰好一次 `pid=<PID>`（`PID > 1`）记录；未知、重复、空、
尾随和 denied/PID 冲突字段都是 typed failure，PID 还必须是无符号 ASCII 十进制，不是“没有 descendant”。child recovery
report 也必须是一个包含已知 marker 与恰好一次有效 PID 的完整记录。report 缺失、空内容、
无 PID 或非法 PID 都是 typed failure，不是“没有 descendant”。
只有 worker 明确写出 `setsid-denied=true` 的预先分支才可证明 no descendant。其余
report、read、parse、capture 或 identity cleanup 任一失败都会经同一个 bounded finalizer
升级固定 failure state 并保留 evidence；若 failure upgrade、证据恢复或 detached
descendant cleanup 不能证明完成，先持久化固定 evidence 再 abort。只有 direct reap、
escaped identity cleanup 和目录 fsync 都确认后才注销。若身份无法捕获，证据不会伪装成
clean，交由 residual scan 继续 fail-closed。native unit test 还会以独立子进程进入真实
manifest/escape finalizer，注入“post-unlink directory fsync → restore write → failure
evidence”链路并观察固定 abort；父测试只验 failure marker、原始 pending evidence 和
bounded process-group reap，不在测试进程内直接 abort。

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
private_root="${JA_SANDBOX_PRIVATE_ROOT:-$RUNNER_TEMP}"
cargo +1.88.0 run --manifest-path spikes/sandbox/macos/Cargo.toml --bin marker_cleanup --locked -- --residual-scan --root "$private_root" --report "$RUNNER_TEMP/ja-sandbox-process-table.log"
```

CI 的 marker-cleanup fixture 由同一个 `marker_cleanup` Rust binary 执行 forged、
pending、descendant 与 `EPERM` 分类断言；Bash 只负责传递 runner 参数，不复制进程
解析或 signal 逻辑。因此脚本 fixture 的通过不能替代下方原生 Seatbelt probe。

其中最后两条只能在 macOS 原生环境运行；非 macOS 明确失败，不能改成 skip。

CI 的原生 security test 即使第 1 轮失败也会继续采集第 2、3 轮；direct probe、helper
cleanup 和 process-table residual gate 使用 `always()` 继续执行，最终仍以首个非零状态
失败，避免早退掩盖残留证据。

## 不能证明的边界与生产门槛

- 同一普通用户在 profile/path preflight 之后主动竞态创建 symlink/hardlink 的攻击
  不被这个 spike 完整覆盖；正式写回必须携带 expected hash/revision，写前再次校验
  inode、mode、xattr 和内容，并由固定签名 worker resource 执行。
- marker/scope 的实现只在打开 descriptor 后以 `fstat`，并在 unlink 前立即以
  `fstat`+`lstat` 比较 `st_dev/st_ino/nlink/mode/uid`，随后同步父目录；这不是对同一
  UID 外部进程纳秒级 path swap 的完整防护，也不使用危险的无条件 `unlink` hack。其
  管理父目录必须保持应用私有 `0700`，Seatbelt child 无权访问；若部署改变这一权限或
  profile allowlist，必须重新审计，而不能把 descriptor 检查描述为同 UID 隔离。
- Seatbelt 不是 kernel/VBS 级恶意代码隔离，也不替代签名、公证、依赖供应链审计、
  Tauri capability 或 Java provider Secret 隔离。Java core 仍不应整体放进 workspace
  sandbox，只有 Java-owned Tool Worker 进入该边界。
- `/usr/bin/sandbox-exec` 在未来 macOS 版本消失或被限制时必须报告
  `SANDBOX_POLICY_UNAVAILABLE` 并停止 Tool Worker，不能自动切 Full Access。
