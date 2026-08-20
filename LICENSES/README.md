<!-- @author kongweiguang -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# 第三方许可证目录

本目录存放发布审计确认需要随 JA 源码或二进制再分发的第三方许可证正文和 notices。
`approved/manifest.json` 将每个 hash-addressed 正文映射到锁定依赖和固定来源；没有
清晰来源、版本、版权或 NOTICE 证据的文本不能直接放入归档。

当前归档状态以 `approved/manifest.json.status` 为准。`source-verified-pending-legal-review`
仍会阻塞发布；只有显式 `status=approved` 才能通过供应链门。

发布任务必须从实际 lockfile、Maven/Cargo/npm 解析结果和最终 Native Image/Tauri
产物生成清单，核实每个文件的来源、版本、SPDX 标识、版权和再分发条件后再批准归档。
