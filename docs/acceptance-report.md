# Pi 验收报告 — T8（占位 / 实物待测）

依据 `TECH_SPEC.md` §13、`tasks.md` T8.1–T8.6。本文档不记录任何虚数据；每
项门槛预留“结果：待实测”的空位，待 Pi 3B 上跑完实测后填入。

报告中需要出现的环境字段（每节重复填一次太重复，但报告本身在第一页就
有总装字段）：

| 字段 | 说明 | 示例 |
|---|---|---|
| 主机 | Pi 型号、内存 | Raspberry Pi 3 Model B, 1 GB |
| OS | Raspberry Pi OS 版本与架构 | Raspberry Pi OS Lite 2024-xx-xx, aarch64 |
| 内核 | `uname -r` | 6.6.y |
| 麦克风 | USB 麦克风型号、USB ID | 例：某个常见 USB 麦克风，0d8c:0014 |
| Rust toolchain | `rustc -vV` | rustc 1.xx.x |
| 二进制 | git commit / version | snore-monitor 0.1.0 (commit <sha>) |
| build profile | `cargo build --release` 配置 | opt-level=3, lto=thin |
| 启动日期 / UTC | 开始连续运行的 UTC | 2025-xx-xxTxx:xx:xxZ |
| 样本量 | 连续运行小时数、连续夜晚数、人工标签事件数 | 8 h / 7 nights / N events |

---

## T8.1 目标板编译安装与能力验证 — 对应门槛 1

**门槛**：在目标板编译安装；列出麦克风实际 ALSA formats/rates/channels，验证
协商/回退真实可用。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| 编译耗时 | `time cargo build --release` | （参考） | 待实测 |
| 麦克风能力列表 | `cat /proc/asound/card*/stream0`、CPAL `supported_input_configs` | 与 §5.1 支持表匹配 | 待实测 |
| 协商/回退 | 调整 `preferred_rate_hz` 后重启服务 | 与规格协商顺序一致（配置 → 48k → 44.1k） | 待实测 |
| 启动失败路径 | 不插麦克风 / 多麦克风 | 明确字段级错误启动失败，不静默乱选 | 待实测 |

环境：填写样本量/环境/版本。

---

## T8.2 8 小时连续运行 — 对应门槛 2

**门槛**：连续运行 ≥ 8 小时，录音 + 检测 + HTTP 同时开启；WAV 存活、文件长度、xrun/gap、
CPU/RSS/温度、磁盘写入。目标 RSS < 80 MiB、DSP 平均 CPU < 单核 10%、峰值 < 25%。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| WAV 可播放 | 标准播放器上外部试 | 能从头拖到尾、有声音 | 待实测 |
| 文件长度 | `find recordings -name '*.wav' -printf '%s\n'` | 与 `rate × channels × 2 × seconds` 一致 | 待实测 |
| xrun / gap 计数 | `/api/v1/status` 中的 `xrun_count`、`gap_count`、`lost_frames_total` | 正常环境接近 0；压力下不崩 | 待实测 |
| RSS | `ps -o rss= -p <pid>` | < 80 MiB | 待实测 |
| CPU（平均） | `top -b -n 10 -d 60 -p <pid>` 取均值 | DSP < 单核 10% | 待实测 |
| CPU（峰值） | 同样命令取最大 | DSP < 单核 25% | 待实测 |
| 温度 | `vcgencmd measure_temp` | 不超限（仅记录） | 待实测 |
| 磁盘写入 | `/api/v1/status` `used_bytes`、`du -sh recordings` | 与 rate × seconds 一致 | 待实测 |

环境：填写样本量/环境/版本。

---

## T8.3 时间线精度与时钟 — 对应门槛 7、8、9

**门槛 7**：播放已知节拍声 ≥ 1 小时，比对事件偏移与真值、`clock_drift_ppm`；`stress-ng`
人工压力下检查 xrun/gap、回放对齐、gap 显示。

**门槛 8**：在目标板验证 CPAL/ALSA 是否提供可靠的捕获时间戳；不可用时确认 `gap_detection_limited`
已在状态页显示。

**门槛 9**：NTP 未同步启动（断网开机）：验证 `unsynced` 标注、同步后壁钟阶跃触发轮转与修正。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| 已知节拍事件偏移 | 用手机播放 1 Hz 脉冲音；Portal 中查看事件 offset | 偏差 ≤ 1 帧（10 ms） | 待实测 |
| `clock_drift_ppm` | `/api/v1/status` 取值 | 记录绝对值与趋势 | 待实测 |
| `stress-ng` 下 xrun | `stress-ng --cpu 4 --io 2 --vm 1 --vm-bytes 256M --timeout 1h` | xrun/gap 不引发服务崩溃 | 待实测 |
| `gap_detection_limited` | 查 `/api/v1/status` | 时间戳不可靠时为 true | 待实测 |
| `estimate_source` | 查 `/api/v1/status` | 时间戳可靠时为 timestamp | 待实测 |
| 断网启动 `unsynced` 标注 | 启动后查段 `time_quality` | unsynced | 待实测 |
| NTP 同步后阶跃轮转 | 同步后查段 `time_quality` | corrected；边界轮转 | 待实测 |

环境：填写样本量/环境/版本。

---

## T8.4 回收、故障与恢复 — 对应门槛 5、6

**门槛 5**：低配额触发回收，只删最旧 complete，事件保留并显示过期。

**门槛 6**：拔麦克风、模拟 USB/磁盘故障、`kill -9`、重启，核对状态页与恢复结果。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| 低配额回收 | 临时调小 `recording_max_bytes` | 只删最旧 complete，事件保留 | 待实测 |
| `storage_pressure` | `/api/v1/status` | 超限时 true | 待实测 |
| 拔麦克风 | 热拔 30 s | 服务不崩；重插后 30 s 内恢复 | 待实测 |
| USB 存储拔出 | 拔 1 分钟 | 服务不崩；录音停；重插后恢复 | 待实测 |
| 磁盘满 | `dd` 写满 / 限制 quota | `storage_pressure=true`，状态页告警 | 待实测 |
| `kill -9` | `kill -9 <pid>` | 启动恢复扫描识别 .partial / orphan / missing | 待实测 |
| 重启 | `systemctl restart` | 启动恢复扫描后录音继续 | 待实测 |

环境：填写样本量/环境/版本。

---

## T8.5 Portal 移动端实测 — 对应门槛 4

**门槛**：手机浏览器回放、拖动、候选定位、小时文件切换。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| 桌面浏览器回放 | Chrome / Firefox | 点击事件 → 选中、加载、seek | 待实测 |
| iOS Safari 回放 | iPhone | 同上 | 待实测 |
| Android Chrome 回放 | Android | 同上 | 待实测 |
| 拖动定位 | 点击事件后手动拖 <audio> 到任意 1 分钟 | 能拖；播放头同步 | 待实测 |
| 跨小时边界 | 跨 00:00 / 06:00 的事件 | 选中正确片段 | 待实测 |
| 录音已过期 | 触发 retention 后 | 显示“录音已过期”，不可播放 | 待实测 |

环境：填写样本量/环境/版本（含手机型号 / iOS / Android 版本）。

---

## T8.6 真实录音评估与阈值校准 — 对应门槛 3

**门槛**：连续多晚采集，人工标注，统计 TP/FP/FN、precision/recall；报告样本数和环境；
不用合成数据推导识别准确率。

| 项 | 测量方法 | 目标值 | 结果 |
|---|---|---|---|
| 连续多晚 | 采集 ≥ N 晚 | N ≥ 7（推荐） | 待实测 |
| 人工标注事件数 | 人工听录音 × Portal 跳转 × PATCH | 报告总事件数与人工标注数 | 待实测 |
| TP / FP / FN | 以人工标注为 ground truth | 数值记录 | 待实测 |
| precision | TP / (TP + FP) | 不定（需现场定） | 待实测 |
| recall | TP / (TP + FN) | 不定 | 待实测 |
| 阈值校准 | 用 `detect-wav` 调参 | 写回配置示例；`detector_version` 更新 | 待实测 |
| 误报片段时间戳 | 保存示例事件 segment + offset | 以供后续调参 | 待实测 |

环境：填写样本量/环境/版本（必填麦克风型号、摆放距离、卧室噪声描述、默认阈值初值、偏离原值的修正）。

---

## 总结（待填）

- 所有门槛是否达标：待实测。
- 需要调参 / 需要重写的部分：待实测。
- 阈值与 `detector_version` 最终取值：待实测。