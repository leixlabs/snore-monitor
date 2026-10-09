# T0.1 规格遗留决策（依据 `TECH_SPEC.md` v0.6、T0.1）

T0.1 在 §6/§8/§9/§11 里遗留了五项决策。本文件是这些决策的正式记录，并
同步了 `TECH_SPEC.md` 的最小化修改（本文件的修改）以满足 T0.1 的完成标
准（"在规格中有明确文字"）。

## 决策一览

| # | 主题 | 决策 | 同步到规格 |
|---|---|---|---|
| 1 | 日期与时区 | `?date=` 按配置的 `[server] timezone`（IANA 名；留空使用设备本地时区）转 UTC 半开区间；跨午夜段与区间相交即返回 | §8 |
| 2 | 容量回收触发时机 | 片段完成后、启动恢复扫描后、以及每 `storage.retention_check_interval_seconds`（默认 60 s）周期检查；写入时若 free space 低于 `recording_reserve_bytes` 且清理无效则先停止录音并告警 | §6、§11 |
| 3 | 网络访问控制 | `bind_address` 默认 `127.0.0.1`，由部署配置放开；PATCH 不加 token，MVP 维持无认证 | §8、§11 |
| 4 | 崩溃一致性 | rename 与 DB complete 之间的 orphan `.wav` 由启动恢复扫描对账；DB 有行无文件置 `missing`（进入：恢复扫描发现文件缺失；退出：不自动退出，人工处理后由恢复扫描或人工更新）；无法验证头部的 `.partial` 保持原样不公开 | §4.2、§6 |
| 5 | 检测盲区 | 确认接受 MVP 不持久化 `detector_drop`（§4.3 已是限制，本决策明确为“接受”） | §4.3 |

## 1. 日期与时区

**背景**：§8 的 `?date=YYYY-MM-DD` 需要决定按哪个时区转 UTC、跨午夜段
归属哪一天。

**决策**：

- 在配置里加 `[server] timezone`（IANA 名，默认 `""` = 设备本地时区）。
- 请求中的日期表示“该时区下的日历日”，按 `[start_of_day_utc, start_of_next_day_utc)`
  半开区间查询；跨午夜的段若与之相交即返回，不按 “属于哪个本地日历日” 过滤。
- `server.timezone` 非空时必须能被 `chrono-tz` 解析为有效 IANA 名；留
  空则不做运行时解析，调用链使用主机本地时区。

**`TECH_SPEC.md` 同步位置**：

- §8 表下面追加一段说明“`?date=` 按 `[server] timezone` 转 UTC，跨午夜段与区间相交即返回”。

## 2. 容量回收触发时机

**背景**：§6 只说“每片段完成后、进程启动恢复扫描时检查”，需要明确是否
需要周期检查、写入中如何处理 free space。

**决策**：

- 触发点 = 片段完成 ∪ 启动恢复扫描 ∪ 周期计时（`storage.retention_check_interval_seconds`，
  默认 60 s）。这三者任一发生都会检查当前总量，超 `recording_max_bytes` 则
  按时间升序删 complete 文件至 ≤ `recording_cleanup_target_bytes`。
- 写入路径上，如检测到文件系统 free space 低于 `recording_reserve_bytes`，
  先触发一次清理；如清理后仍低于 reserve，**先停止录音**并发出告警（状态
  页 + 日志），不静默丢样本、不写入根分区。

**`TECH_SPEC.md` 同步位置**：

- §6 顶部加上“周期检查”与“低 reserve 时停止录音”两句。
- §11 示例添加 `retention_check_interval_seconds = 60` 一项并加一行
  校验说明。

## 3. 网络访问控制

**背景**：§11 示例里 `bind_address = "0.0.0.0"`，与 MVP “限可信局域网”
的表述不一致；§8 写“MVP 无认证，不开放公网”，需要明确默认与 PATCH 上限。

**决策**：

- `server.bind_address` 默认 `127.0.0.1`（已在 `src/config.rs` 中以
  `default_bind_address()` 锁定）。LAN 部署必须显式编辑配置，由部署
  者承担 §2/§8 的威胁责任。
- PATCH `/events/{id}` 仍不加 token；MVP 维持无认证、限可信局域网
  访问、不公网映射。

**`TECH_SPEC.md` 同步位置**：

- §11 示例的 `[server]` 表中 `bind_address = "127.0.0.1"`（默认同），
  与 §10 / §8 一致。
- §8 末尾加一句“默认仅本机访问；LAN 部署需修改 `bind_address`；**不要
  映射到公网**”。

## 4. 崩溃一致性

**背景**：§4.2 只说了 `.partial` 在异常退出下的处理；未说 `.partial` ↔ DB
对账、`missing` 状态语义。

**决策**：

- rename 与 DB `complete` 之间崩溃：`.wav` 已是原子名，但 DB 行可能
  仍为 `recording`。启动恢复扫描扫出 DB `status=recording` 但 `.partial`
  已被重命名为 `.wav` 的情况：以文件实际状态为准重写 DB `status=complete`，
  并以文件大小重算 `size_bytes` / `frame_count`。
- 孤儿 `.wav`（文件存在、DB 无行）：启动恢复扫描创建对应 `recording_segments`
  行， `status=complete`，`time_quality` 设为 `unsynced`，在 `/status` 与日志
  中发出告警。
- DB 有行无文件：恢复扫描将 `status` 置为 `missing`。**进入条件**：恢复扫描
  发现文件路径不可访问。**退出条件**：不自动退出，需人工干预（恢复文件
  后由人工调用 PATCH 或重新跑一次恢复扫描以重新评估）。
- `.partial` 无法验证头部（与实际数据不一致）：保持原样不公开，不重命名，
  不进 DB；状态页告警。

**`TECH_SPEC.md` 同步位置**：

- §4.2 末尾补充一段 `missing` 状态语义与“进入/退出条件”。
- §6 补一句“崩溃恢复扫描对账 orphan / missing”。

## 5. 检测盲区

**背景**：§4.3 列为 MVP 已知限制，需要 T0.1 明确“接受”。

**决策**：**接受** MVP 不持久化 `detector_drop`。Portal 不显示“该时段
检测器未工作”是已知限制；状态页有 `dropped_detection_blocks` 计数与
`gap_detection_limited` 指示，但不试图在时间轴上画出检测盲区。后续如需
需开新的会议重新设计（可能影响准确率评估与 API）。

**`TECH_SPEC.md` 同步位置**：

- §4.3 “已知限制”句末加一句“已确认接受（T0.1）”。

## 同步修改清单

按本决策修改 `TECH_SPEC.md` 的位置如下（每处变化都对应本文件中一条决策）：

| 决策 | 规格小节 | 修改 |
|---|---|---|
| #1 日期与时区 | §8（HTTP API） | 在 API 表下“参数严格校验”句后增加一条“日期与时区（T0.1）”项，说明按 `[server] timezone` 转 UTC 半开区间、跨午夜段与区间相交即返回 |
| #2 容量回收触发时机 | §6（存储与自动回收） | “每片段完成后”一句改为描述三个触发点（片段完成、启动恢复、周期检查）并明确低 reserve 时先停止录音；末尾增加一条“崩溃恢复扫描对账” |
| #2 配置键 | §11（配置示例） | `[storage]` 增加 `retention_check_interval_seconds = 60`；校验段增加 `server.timezone` 须能为 `chrono-tz` 解析、`retention_check_interval_seconds > 0` |
| #3 bind_address 默认 | §11（配置示例） | 示例值从 `"0.0.0.0"` 改为 `"127.0.0.1"`，并加注释“默认 loopback；LAN 部署需手动放开，不要映射到公网（T0.1）” |
| #3 公网映射警示 | §8（HTTP API） | “MVP 无认证”句末尾补“`bind_address` 默认 `127.0.0.1`；LAN 部署需手动修改配置并不要映射到公网” |
| #4 崩溃一致性 | §4.2（WAV 录音与时间映射） | 末尾增加一条“崩溃一致性（rename 与 DB `complete` 之间崩溃）”说明 orphan / `missing` 进入与退出条件 |
| #4 对账 | §6 | 末尾增加一条“崩溃恢复扫描对账”说明 orphan / `missing` 处置 |
| #5 检测盲区 | §4.3 | “已知限制”句末加“（T0.1 已确认接受）” |

修改中**不**改动其他章节、不重排；仅在已有句子/示例后补充最小必要文字。`git diff --stat TECH_SPEC.md` 结果：1 file changed, 10 insertions(+), 5 deletions(-)。