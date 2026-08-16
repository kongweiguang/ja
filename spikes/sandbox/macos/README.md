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
- 为兼容 macOS 的 dyld/firmlink 与标准库启动，profile 仅增加固定的 system-only
  `vnguard`/Sandbox syscall、`opendirectoryd`/`secinitd` lookup、系统根组件
  metadata、`/bin`/`/usr/bin` 等只读 loader paths 及 `/dev` stdio 设备；这些规则不
  允许 workspace/tmp executable mapping，也不开放 network 或 unrestricted process control。
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
  group/direct PID，且只有 direct reap 与 group `ESRCH` 同时成立才把 active marker
  转入 cleaned/recovery 状态；预存在的恢复 evidence 仍由同一有界 GC 负责。
  marker cleanup 会先打开并校验 owner-private root directory，再通过持有的
  Darwin `fdopendir/readdir` 扫描和 `openat` 重新打开 marker，删除则使用同一个
  dirfd 的 `renameat/unlinkat`；因此授权扫描、marker open 与删除不会各自解析一份
  可替换的 root pathname。active marker 会先在该目录内原子改名为
  `.ja-sandbox-cleaned.*`，并在同一目录内写入 `.ja-sandbox-recovery.*` 原始镜像后
  fsync；目录描述符先尝试 Darwin `F_FULLFSYNC`，对目录不支持该 regular-file
  操作时仅回退到 `fsync(dirfd)`，普通文件仍使用文件级 `F_FULLFSYNC`；两种目录
  同步都失败都会 fail-closed。同一轮保持完整 backup 直到 recovery 的最终 unlink 与目录 fsync 都成功，
  然后才删除 backup，所以成功的 active cleanup 不留下 cleaned/recovery evidence。
  任一最终 unlink 后的
  fstat、root revalidate、directory sync 或 deadline 失败，都会用持有的完整 image
  通过 O_EXCL 恢复原 recovery 名称或可识别的 cleaned 别名；两者都无法恢复时固定
  fail-closed abort，不能把证据丢失当成普通成功。恢复候选在另一份完整 image
  已通过 regular-file/0600/uid/nlink、rootfd identity 和 parent-directory sync 前
  不会被删除；两份候选都失败时保留现有最可信 inode 并 abort。下一轮 cleanup 仍能
  识别恢复出的有限状态证据。若首候选短写或候选 unlink/postcheck 失败，扫描会把
  它标记为受控 basename 下的 damaged sibling；只有重新打开、完整重解析并同步
  有效 sibling 后，才允许 fd-relative 隔离该 inode。无有效配对证明的
  `marker-stat-invalid` 永远保留并使 gate 失败，不会被“清理”吞掉。workflow 最终要求 unresolved evidence 为零，扫描和
  pending 数量仍受 64 项上限约束。不会把 active marker 清零却丢掉唯一可恢复证据。
  `.marker.pending`、`.fallback.pending` 和 `.emergency.pending` 使用独立的有限状态机：
  先在 rootfd 下发布并同步 `.ja-sandbox-recovery.<pending>` 或
  `.ja-sandbox-cleaned.<pending>`，再删除原 pending，最后删除保留副本。任何写入、
  unlink、postcheck、root revalidate 或目录 fsync 失败都会通过同一 rootfd 事务把
  最后一份完整 image 重新以 O_EXCL 发布，留下这两个固定 grammar 可识别的状态；不会
  把嵌套 pending 名称送入 active marker 的 `recovery_backup_names`，也不会放宽 active
  basename grammar。只有最后一次删除及其 root/目录 durable postcondition 全部成功时才
  返回零 unresolved；正常完成同轮删除所有 pending alias，重启扫描可继续收口。
  每次 cleanup 从扫描前创建一个 monotonic 20 秒 deadline，并贯穿 root metadata、目录
  entry、每个身份查询 stdout/stderr、signal/wait、marker unlink、目录 sync 和最终
  report；不为每个 marker 或 fixture cleanup phase 重启窗口。身份查询 stdout/stderr
  共享 1 秒绝对 deadline，各自 4 KiB 上限；
  query helper 的 `Child` 所有权在 direct reap 被确认前不会释放，try_wait 错误或
  bounded reap 失败会继续 kill/reap，仍无法证明时显式 fail-closed abort。fixture
  launcher 的 pipe、输出、PID 解析和后续 `?` 传播也统一经过同一个有界 Child
  finalizer；二次 reap 仍失败时固定输出安全类别并 abort，不让 live Child 进入 Drop。
  所有 native fixture 从 `run_fixture` 入口创建同一个 8 秒 monotonic budget；forged、
  pending、residual、descendant 的 pipe 读取、身份查询、failure evidence、生产 cleanup、
  launcher reap 与错误 finalizer 全部共享它；临近 deadline 时不再删除 marker/evidence，最终 report 也可能来不及
  写入，但已有 primary/fallback/emergency 或 fixture failure evidence 会保留供恢复。
  fixture failure evidence 也只通过已校验的 owner-private root fd 写入：先以
  `O_EXCL|O_NOFOLLOW|0600` 创建 recovery 与 pending sibling，完整写入并 fsync，校验内容、
  mode、uid、nlink 后原子 `renameatx_np(RENAME_EXCL)` 发布 final，再 fsync 同一目录；
  目标已存在时不会覆盖 good final。write、file fsync、
  rename 或目录 fsync 失败都保留可解析 sibling；重启时优先保留既有 good final，或从
  pending/recovery 原子恢复 final，绝不覆盖 good final 或删除最后一份证据。
  每份 evidence 必须是完整的 version-2 固定 14 行 grammar：version、允许的 category、
  supervisor 的 state/pid/pgid/uid/comm/start，以及 target 的同一组字段，字段顺序固定且
  不允许重复、未知、空值、缺行、尾随内容或截断。`known` 必须有当前 UID 和有效 PID/PGID；
  `comm/start` 在值含路径或其他不安全字符时可固定为 `redacted`，不影响内存中已捕获
  的 UID/PID/PGID 身份校验；
  `provisional` 只能保留大于 1 的 PID/PGID，并将 UID/comm/start 固定为 unknown/redacted；
  `unavailable` 只能保留可验证的大于 1 direct PID 或全 unknown，不能伪造 PGID/身份。PID、
  PGID 还必须互不冲突，且 supervisor PID↔target PGID、supervisor PGID↔target PID 两种交叉
  相等也拒绝；同时拒绝当前进程/进程组和保留值。短或状态不一致的 final/pending/recovery
  只保留为诊断证据，不能提升为 final；无效 ordinary final 会在同一 root-fd 下以
  `renameatx_np(RENAME_EXCL)` 隔离到两个固定 bounded damaged 名称之一，既不覆盖已有 damaged
  evidence，也不任意遍历新名称；随后 pending/recovery 只有完整 sibling 才能提升。两个 damaged
  名称均被占用、隔离/目录 fsync 失败或没有完整 sibling 时保持 fail-closed。pending 无效但
  recovery 完整时只提升完整 sibling，并保留无效 pending 供诊断。
  fixture 的 descendant group 在每个早期失败返回前再执行有界 SIGKILL/ESRCH
  收口；fixture supervisor 通过同一 Rust binary 的受控模式持有 target Child，异常时先
  发送固定控制字节让 supervisor 直接 kill+wait target，再由父进程以自己的 Child
  ownership bounded reap supervisor，并独立复核 supervisor 的 PID/PGID 已消失。正常
  descendant marker cleanup 在生产 group signal 成功后立即通过同一 absolute deadline
  写入一次 `q`，并等待 supervisor 返回唯一的 `target-reaped=true` acknowledgement；
  只有该 ack 明确证明 target direct reap 与 target PGID empty 后，production 才继续
  direct-PID/PGID residual 复核。ack 缺失、格式错误、supervisor crash 或 query race 都
  保留 marker/evidence 并 fail-closed；ACK reader 会在同一 deadline 内继续 drain 到
  supervisor terminal/EOF，chunked 合法行才接受，任何第二行、尾随字节、超量输出或
  nonzero status 都失败。父端随后只等待/reap supervisor 并再次独立复核
  target/supervisor，绝不会在 ack 被消费前直接杀 supervisor。
  ACK 成功后，生产状态机继续使用已捕获的 target identity 只做 PID/PGID
  disappearance 复核；即使 Darwin 短暂把 direct PID 报为 present，也等待其
  gone/residual 结果而不对预期已 reap 的 PID 重新执行 `ps` identity query。
  ACK proof 本身携带当次 captured target 的 PID/PGID/UID/comm/start identity，
  并且在 fixture 协议中只消费一次；重复回调或后续不同 marker group 只能回到
  正常 identity revalidation，绝不会复用第一个 target 的全局布尔状态。相同 PID
  但 start/comm/UID/PGID 任一变化也不匹配 proof。
  这样被
  signal 的 target 不会因 launcher 同时死亡而留下未回收 zombie；控制 EOF、非 `q`、
  读错或 nonblocking 设置失败也会先收口 target，再以固定非零/abort 结束，绝不当作
  成功。supervisor/target 的身份与 PGID 分开记录，父端必须同时拥有 target 的外部
  identity 与 supervisor 的独立 identity；缺少任一身份只保留 failure evidence 并
  fail-closed，绝不把 supervisor 的 PID 当作 target marker。若 supervisor identity
  查询失败，父端先读取 target 的 provisional PID/PGID 并持久化受限 evidence，发送 `q`
  后只等待 supervisor 自然退出；只有 supervisor 成功状态（其内部已确认 target
  direct reap+PGID empty）或 target 已取得外部 identity 后，才允许完成该失败分支的
  bounded cleanup，绝不在 q 尚未消费前直接 kill supervisor。父端向控制管道写入 `q`
  前先设置 nonblocking，并在同一 absolute deadline 内处理 `WouldBlock`，不使用无界
  `write_all`/flush。控制管道、residual、EPERM
  或其他 probe 错误固定映射为 abort 类别，不能依赖
  尚未创建的 marker 或 workflow glob 推迟清理。descendant fixture 读取同一次生产 cleanup report 时要求完整、有界、ASCII
  且严格匹配 `category=true`（唯一、按生产顺序）和最后一个 `marker-count=<n>`；未知、
  路径、重复、冲突、`false`、空行、尾随内容、超限或缺失 count 都是
  `fixture-descendant-cleanup-report`，不会把具体 PID、路径或系统错误文本带入 CI。
  合法报告才映射为固定的 `fixture-descendant-cleanup-{residual,eperm,signal,query,
  identity,remove,scan,unsafe}` 类别。
  目录 entry
  读取错误会清空已收集的 signal target 并报告固定 `marker-entry-invalid`，不会静默
  忽略不完整扫描。group cleanup 最长 20 秒，workflow 外层还设置 2 分钟 timeout，不能
  把 timeout 当作已确认的 reap 或 `ESRCH`。
  一次 marker cleanup 共享一个 monotonic 20 秒 deadline，并在扫描阶段限制最多 128 个
  directory entries，最终最多处理 64 个 active marker、每组 32 个 PID、64 个 pending
  marker；不会为每个 marker 重新开启 20 秒窗口。
  marker 去重键包含 owner/nonce、PID/PGID、suffix 和打开时的 inode identity，避免不同
  后缀或替换 inode 被误跳过。每次 signal 前还校验 PID/PGID、start identity、comm 和
  当前 UID；保留值、当前 supervisor PID/PGID 以及其他 UID 一律 fail-closed，root
  也不能向其他 UID 发 signal。
  marker-cleanup descendant fixture 在原生 macOS runner 上由 supervisor 创建一个独立
  target PGID，并只为 target 保留真实 PID/PGID/comm/start identity marker；supervisor
  留在自身 group 并负责 wait/reap target。若 group signal 返回 `EPERM`，生产 fallback
  会逐个重新校验 UID、PID、PGID、comm 和 start identity 后 direct-signal，并要求 target
  与最终 PGID 都报告 `ESRCH`，随后才删除 marker。runner 未产生 `EPERM` 时，仍由 errno
  fault table 保持 fail-closed 回归，不把成功 group signal 冒充 fallback 覆盖。
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
只有 worker 明确写出 `setsid-denied=true` 的预先分支才可证明 no descendant；在当前
macOS acceptance runner 上该分支会作为明确的平台能力失败返回，不能替代真实
`setsid-started=true` escaped-session 负例。其余
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
- marker cleanup 会持有已校验的私有 root directory descriptor，以 `unlinkat` 锚定目录，
  并在 unlink 前立即以 `fstat`+`lstat` 比较 `st_dev/st_ino/nlink/mode/uid`，随后同步父目录；
  这不是对同一
  UID 外部进程纳秒级 path swap 的完整防护，也不使用危险的无条件 `unlink` hack。其
  管理父目录必须保持应用私有 `0700`，Seatbelt child 无权访问；若部署改变这一权限或
  profile allowlist，必须重新审计，而不能把 descriptor 检查描述为同 UID 隔离。macOS
  没有 pidfd 式的 query→signal 原子承诺，因此同一 UID 恶意宿主在最后身份复核后仍
  可能制造理论 PID 复用；这里仅以 UID、start identity、comm、PGID、signal 前复核、
  unknown peer fail-closed、数量上限和统一 deadline 缓解，不能宣称绝对防护。
- Seatbelt 不是 kernel/VBS 级恶意代码隔离，也不替代签名、公证、依赖供应链审计、
  Tauri capability 或 Java provider Secret 隔离。Java core 仍不应整体放进 workspace
  sandbox，只有 Java-owned Tool Worker 进入该边界。
- `/usr/bin/sandbox-exec` 在未来 macOS 版本消失或被限制时必须报告
  `SANDBOX_POLICY_UNAVAILABLE` 并停止 Tool Worker，不能自动切 Full Access。
