# Raspberry Pi 打鼾监测系统 — 技术规格书

**版本**：0.6（MVP）  
**状态**：可供实现  
**项目目录**：`~/dev/projects/2610-snore-monitor-pi/`  
**目标设备**：Raspberry Pi 3 Model B（1 GB RAM）+ USB 麦克风 + USB 存储设备  
**使用场景**：个人卧室、局域网访问；连续保存原始录音、规则式鼾声候选检测、Portal 回放和事件核验。

---

## 1. 产品目标

构建本地睡眠声音监测系统：树莓派持续保存 USB 麦克风的原始 PCM 录音；轻量规则产生疑似鼾声时间段；Portal 按日期显示录音覆盖、鼾声候选时间轴并可回放；录音超过配置上限时自动删除最旧完整录音片段。

本系统用于个人观察和算法验证，不是医疗器械，不提供睡眠呼吸暂停或其他疾病诊断。

### MVP 成功标准

- 设备重启后服务自动启动；USB 麦克风正常连接时持续录音。
- 保存未滤波、未裁剪、未有损压缩的原始 PCM WAV，Portal 可回放、拖动定位。
- 页面标记检测到的候选鼾声时间段，可跳转到录音相应位置，用户可标记鼾声/误报。
- 检测故障不阻断录音；音频采集缺口、xrun、写盘失败必须在状态页和日志中可见。
- 存储到达上限时按时间删除最旧的已完成片段，不删除当前写入片段。
- 局域网客户端可访问 Portal/API；MVP 不开放公网。

## 2. 范围与非目标

包含：ALSA USB 音频采集、PCM WAV 分段、轻量 DSP 规则检测、SQLite、HTTP REST、音频 byte-range 回放、静态 Portal、容量回收、systemd 服务、测试。

不包含：云端上传、原生手机 App、账号系统、公网访问、深度学习模型、医学诊断。

## 3. 目标硬件与资源预算

| 项目 | 规格 |
|---|---|
| 主机 | Raspberry Pi 3 Model B，ARMv8，1 GB RAM |
| OS | Raspberry Pi OS Lite，优先 64-bit；按依赖兼容性在目标板验证 |
| 音源 | USB 麦克风，ALSA capture |
| 采集格式 | 启动探测设备能力，保存设备协商到的原始 PCM 格式；优先 mono S16_LE，采样率依次尝试配置值、48 kHz、44.1 kHz |
| 检测格式 | 统一 16 kHz mono S16，必要时抗混叠重采样 |
| 服务 | Rust 2021/2024（按依赖 MSRV 锁定）原生二进制，由 systemd 托管；无 Python VM/GC |
| HTTP | Rust Axum + Tokio；小并发配置，音频端点实现 HTTP Range |
| DSP | `dasp` 无分配基础组件、`biquad` IIR、`rubato` 按需重采样；仅引入实际需要模块。[2][3][4] |
| 音频输入 | CPAL/ALSA（最终依赖以树莓派上设备探测稳定性验证为准） |
| DB | `rusqlite` / SQLite；若启用 bundled SQLite 仍需测二进制及内存 |

这里的“独立二进制”指不需要安装 Python/Go runtime；Linux 音频仍依赖内核 ALSA 与目标系统的基础 libc/ALSA 库。Rust 编译不会自动保证比 Go/C++ 更省资源，优化重点是小型依赖、无每帧分配、低线程数和 buffer 复用。

资源目标（须由 Pi 实测验收，不作为未经测试的承诺）：应用常驻 RSS < 80 MiB；检测与重采样平均 CPU < 单核 10%、短时峰值 < 25%；音频块队列总内存 < 1 MiB（≤48 kHz、≤2 通道、S16 时：capture ring 0.5 s + 录音队列 2 s + 检测队列 1 s = 3.5 s ≈ 0.64 MiB；更高规格须按秒数重新核算）；整夜录音不因 HTTP 浏览或 DB 操作丢帧。音频基础 DSP 仅需约 16,000 samples/s。节律特征如开启，应低频更新并测量成本。

存储：16 kHz mono S16 约 115.2 MB/小时；48 kHz mono S16 约 345.6 MB/小时；44.1 kHz mono S16 约 317.5 MB/小时（十进制 MB，未含文件系统开销）。实际占用以 ALSA 协商格式为准。

## 4. 架构与线程模型

```text
USB Mic → CPAL callback → capture ring (SPSC, 500 ms, 仅复制)
                              │
                        dispatcher 线程：frame 计数 / 分段 / gap 登记 / 时间戳 / 控制消息
                              ├─ recorder worker → raw PCM WAV(.partial→.wav) → recording disk
                              ├─ DSP worker → 选通道 → 16 kHz 重采样 → 10 ms 特征 → 状态机 → snore events
                              └─ DB writer 线程 ← SegmentStarted / audio_gaps / events（批量、异步）→ SQLite
                                                                               │
Browser ← HTTP/Axum+Tokio ← REST + Range audio + static Portal ───────────────┘
Background retention worker: 完整片段轮转、上限检查、最旧文件回收

控制消息（SegmentStart/End/Gap）走保留槽，永不丢；音频块可在队列满时按 4.1 规则丢弃并登记 gap。
```

## 4.1 音频输入（CPAL/ALSA）

- 使用 Rust CPAL 的 ALSA backend 采集 USB 麦克风；callback 仅复制预分配 PCM block 到有界队列，不执行 DSP、数据库、文件或 HTTP。CPAL 为低层跨平台音频 I/O，Linux 使用 ALSA backend 且构建需要 ALSA 开发文件；目标 Pi 上必须实测设备配置及 xrun 恢复。[1] 若 CPAL 对设备/格式控制不足，再退到 Rust `alsa` crate 直接调用 ALSA API，不重写 DSP。
- 设备配置枚举通过 CPAL `supported_input_configs()` 检查支持的 format/rate/channels，依配置优先级选择并创建 input stream；若所需组合不存在，尝试下一个明确列出的配置，全部失败则启动失败并记录设备信息。若设备仅支持多通道，按配置选定通道，不默认混音。
- ALSA period 默认 10 ms、buffer 默认 200 ms（通过 CPAL 可控部分请求这些参数，但硬件可能选择近似 buffer，启动时记录实际配置）。**实际块长由 CPAL/ALSA 决定且可能变化，下游（重采样、组帧、分段）一律不得假设固定块长。** 输入 callback 错误/xrun 必须计数并登记为 gap（见 4.3），必要时重建 stream；不得虚构连续录音。
- 线程与数据流：CPAL callback 只做一件事——把 PCM 复制进预分配的 SPSC capture ring（容量 `capture_ring_ms`，默认 500 ms）；ring 满则丢弃该块并累加原子计数 `capture_ring_overflow_frames`，由 dispatcher 转为 gap。独立 dispatcher 线程读取 capture ring，负责 frame 计数、分段切分、gap 登记、时间戳标注，并向录音队列/检测队列分发（4.3）。
- 每个音频块含：原始 PCM、`capture_seq`（自本次 stream 启动起的累计 frame 序号）、frame 数、单调时钟起始（ns）、UTC 映射、format、`segment_id`、`segment_offset_frames`（块首帧在该 WAV 已存帧序列中的位置）、`gap_before_frames` 与 `gap_kind`（本块之前的不连续；无则为 0/none）。录音和检测各自通过有界队列接收。
- 队列容量：录音 `recording_queue_ms`（默认 2 s）、检测 `detection_queue_ms`（默认 1 s）。
  - 检测队列满：丢弃该检测块，递增 `dropped_detection_blocks`，并在下一个成功入队的块上设置 `gap_before_frames`（kind=`detector_drop`），使检测器按 5.4 重置；WAV 不受影响。
  - 录音队列满或磁盘写入落后：立即告警；丢弃该块，累计 lost frames 并记在下一个成功入队块的 `gap_before_frames`（kind=`queue_overflow`），**该块同时不送检测**，使检测侧得到同样的 gap；gap 写入 `audio_gaps`。不得静默丢样本。

### 4.2 WAV 录音与时间映射

- 保存设备原始 PCM，不在录音支路重采样、滤波、增益或裁剪。WAV header 按真实协商采样率、通道数、位深编写。MVP 输入为 S16_LE；其他格式不支持则启动失败，不得错误标注 WAV。MVP 仅支持 1–2 通道（`WAVE_FORMAT_PCM`）；>2 通道需要 `WAVE_FORMAT_EXTENSIBLE`，MVP 视为不支持并启动失败。
- 每 1 小时新建一个 WAV，开发/测试可配置 10 秒分段。路径：`recordings/YYYY/MM/DD/<UTC-start>_<segment-id>.wav`。
- 先写 `.partial`，以 64 KiB 批次顺序追加 PCM；每 `header_flush_interval_seconds`（默认 30 s）回写一次 RIFF/data 长度并 `fdatasync`，使异常中断最多丢失约该时长，且头部与已落盘数据一致。关闭时更新 RIFF/data 长度，`fdatasync`，原子 rename 为 `.wav`，再提交 DB 状态 complete。WAV classic RIFF 4 GiB 上限；切片时长和采样率必须确保小于 4 GiB，超限提前轮转。
- 事件偏移以原始录音 sample frame 为基准，可由检测时间映射换算。页面播放通过 HTTP Range seek，无需把音频读进内存。
- 服务异常退出的 partial 标记 interrupted，不得标为 complete。启动恢复扫描：对 `.partial` 按 `block_align` 向下取整截掉尾部不完整帧，并按实际文件大小修正头部，校验通过后重命名为 `.wav`，DB 置 `interrupted`，可回放但 Portal 显示“异常中断”；无法验证头部与数据一致的 `.partial` 保持原样、不公开，并在状态页告警。恢复策略不得假称其尾段一定完整。

### 4.3 时间线、缺口与分段协调

**权威时间线 = 已存储 PCM 帧序列。**

- WAV 内容是实际采集到的帧按顺序拼接，**不插入静音填补缺口**（那会虚构录音）。`frame_count`、事件的 `*_offset_frames`、HTTP seek 位置全部基于“已存储帧”，播放器位置 = `offset_frames / capture_rate_hz`。
- 缺口（gap）：已存储帧序列在 `offset_frames` 处与真实时间不连续，丢失约 `lost_frames` 帧。壁钟时间映射：

  ```text
  wall(o) = segment.started_at_utc + (o + Σ lost_frames[gap.offset_frames <= o]) / capture_rate_hz
  ```

  Portal 时间轴据此绘制，gap 处显示为断点；播放头做反向映射。事件的 `started_at_utc/ended_at_utc` 用同一公式写入，保证与回放一致。
- 缺口来源：

  | kind | 触发 | lost_frames 估算 |
  |---|---|---|
  | `xrun` / `stream_error` | CPAL error callback，或相邻块捕获时间戳不连续 | 相邻块差分：`round((t_start − t_prev_end) × rate)`，仅当差值 > `gap_tolerance_ms`（默认 20 ms）时登记 |
  | `capture_ring_overflow` | callback 无法入 capture ring | 被丢弃的帧数（精确） |
  | `queue_overflow` | 录音队列满或写盘落后 | 被丢弃的帧数（精确） |
  | `stream_rebuild` | 重建 stream（含 USB 拔插） | 重建前后单调时钟差 |

  估算不得使用累计时钟比较：USB 麦克风时钟与系统时钟有漂移（数十 ppm 即约 0.2 s/小时），只做相邻块差分。若目标 Pi 上 CPAL/ALSA 不能提供可靠的捕获时间戳，gap 检测降级为仅依赖 error callback 和溢出计数，`estimate_source='counter'`/`'unknown'`，状态页显示 `gap_detection_limited=true`，不得声称无缺口。此项须在 Pi 上实测（见第 13 节）。
- `detector_drop`（检测队列满）只影响检测，不影响 WAV，不写入 `audio_gaps`；仅计数、写日志（含 segment 与偏移），并触发检测器重置（5.4）。**MVP 不持久化检测盲区，Portal 无法显示“该时段检测器未工作”，这是已知限制。**

**时钟与 UTC**

- Pi 3B 无 RTC。开机后 NTP 同步前壁钟不可信。dispatcher 记录 `(CLOCK_MONOTONIC, UTC)` 锚点（启动时及每 60 s 一次）及 `time_synced`（来自 `adjtimex`/`timedatectl`）。
- 段起始 UTC = 该段首帧对应的锚点换算；`time_quality`：`synced` / `unsynced` / `corrected`。
- 检测到壁钟阶跃（相邻锚点的 UTC 差与单调差之差 > 1 s）时：强制轮转分段，使每段只有一个线性时间映射；若阶跃前的段为 `unsynced` 且同一次开机（`boot_id` 相同），按阶跃量修正其 `started_at_utc/ended_at_utc` 并置 `corrected`（文件路径不改名，以 DB `relative_path` 为准）。
- 段内相对时间精度约 10 ms（检测帧粒度）；绝对壁钟误差受 USB 时钟漂移和 NTP 影响，不承诺 10 ms。`ended_at_utc` 取关闭时的锚点换算，`clock_drift_ppm = (frame_count/nominal_rate − 单调经过时间) / 单调经过时间 × 10⁶` 写入段元数据并在状态页展示。

**分段与有序协调**

- dispatcher 独占分段决策，recorder 与 detector 都只响应带序号的有序消息：`SegmentStart{segment_id, started_at_utc, format, time_quality}`、`Audio{…}`、`Gap{kind, lost_frames}`、`SegmentEnd{segment_id, reason}`。
- 分段边界由**已存储帧数**决定：已存 `segment_duration_seconds × capture_rate_hz` 帧即轮转（确定性、不受时钟漂移影响；不强制对齐整点）。强制轮转的原因：`duration` / `gap`（单次 gap 超过 `gap_rotate_seconds`，默认 5 s）/ `time_step` / `format_change` / `size_limit` / `shutdown` / `error`，写入 `recording_segments.end_reason`。
- 控制消息（`SegmentStart/End/Gap`）永不丢弃：队列预留 ≥ 16 个控制槽，与音频块容量分开计算；预留槽也满说明消费者已卡死，dispatcher 不阻塞，设 `degraded` 并由 watchdog 重启该消费者线程（检测器须按 5.4 重置）。
- DB 行创建：dispatcher 在向 detector/recorder 发 `SegmentStart` **之前**，先把 `SegmentStarted` 命令入 DB writer 队列。事件写入发生在 detector 收到该段音频之后，因此因果上晚于 `SegmentStarted`；仍保留保护：外键失败的事件入重试队列并计数 `event_fk_retries`，不得丢弃或崩溃。
- 同一事件不得跨段：`SegmentEnd` 强制关闭候选（`end_reason=segment_end`）；下一段若声音持续，新事件 `continued=1`。
- 设备拔插/重建：stream 出错后以 1 s 起指数退避（上限 30 s）重建。重建成功且格式与当前段一致、gap ≤ `gap_rotate_seconds` 时继续当前段并登记 gap；否则 `SegmentEnd` 后以新格式开新段，detector 重建重采样器并重置。拔掉期间 `microphone=disconnected`，录音状态 degraded，不生成假音频。
- 退出顺序（SIGTERM）：停止 stream → dispatcher 刷出剩余块 → recorder 完成当前段（更新头、`fdatasync`、rename、DB complete，`end_reason=shutdown`）→ detector 刷出 pending 事件（`end_reason=shutdown`）→ DB writer 排空后退出；整体超时（默认 10 s）则放弃并保留 `.partial` 由恢复扫描处理。

## 5. 音频处理详细设计（第一版规则，不用 ML）

原则：原始录音始终旁路无损保存；DSP 仅用于候选检测。算法产生“候选事件”，不是诊断，也不保证能区分所有鼾声和其他声音。算法阈值必须用该麦克风、该卧室的录音校准。

### 5.1 检测支路格式转换

处理链：`dispatcher 块 → 选取检测通道 → S16→f32 → [重采样至 16 kHz] → 5.2 组帧`。

- 通道选择：多通道时先反交织，只取 `mono_channel_index`（必须 < 实际通道数，配置校验失败则启动失败）；不混音。只对该通道做重采样；WAV 仍保存全部通道。
- 归一化：`x = sample / 32768.0`。
- 支持的采集率（其余启动失败，不静默改配置）：

  | 采集率 | 处理 | 比例（输入:输出） |
  |---|---|---|
  | 16000 | 直通，无重采样 | 1:1 |
  | 32000 | 抗混叠重采样 | 2:1 |
  | 48000 | 抗混叠重采样 | 3:1 |
  | 44100 | 抗混叠重采样 | 441:160 |
  | 96000 | 抗混叠重采样 | 6:1 |

  `detection_rate_hz` MVP 固定 16000，配置中写其他值即校验失败。
- 重采样器要求（不绑定具体 crate API，首次实现先验证目标版本能满足）：
  - 使用**精确有理数比**，禁止浮点近似比（44.1 kHz 会累积漂移）；禁止直接抽点。
  - 预分配 buffer，状态跨块保持。CPAL 块长可变，重采样器若要求固定输入块，则在检测线程用累积缓冲按固定 chunk 喂入，不丢尾、不补零。
  - 指标：通带 0–4 kHz 波动 ≤ ±0.5 dB（覆盖鼾声频带）；折返到输出带内的混叠能量衰减 ≥ 60 dB。关键点：输入 14.5–15.9 kHz 会折返到 100–1500 Hz 鼾声频带，必须被抑制。
  - 记录并补偿群延迟 `resampler_delay_in_frames`（来自重采样器自身的 output delay，换算到输入帧），用于 5.1 的偏移映射。
- **帧→偏移映射**：检测器在每次重置/段首建立锚点 `anchor_in_offset`（该处首个输入帧的 `segment_offset_frames`）。检测帧 `k`（10 ms，起点输出样本索引 `160·k`）对应的已存储输入偏移：

  ```text
  offset_in = anchor_in_offset + round(160·k × capture_rate_hz / 16000) − resampler_delay_in_frames
  ```

  事件的 `start/end_offset_frames` 由此计算；跨 gap 不得延续映射（gap 必导致重置并建立新锚点，见 5.4）。16 kHz 直通时延迟为 0。

### 5.2 帧与特征

- 帧长 10 ms、hop 10 ms：16 kHz 下每帧 160 samples。跨块用固定环形缓冲组帧；禁止逐帧动态分配。每帧绑定 `segment_id` 与 5.1 映射得到的偏移，不单独存储时间。
- DC blocker：`y[n] = x[n] − x[n−1] + α·y[n−1]`，α 初值 0.995（截止约 12.7 Hz），状态跨帧/块连续。**重置时以首个样本初始化 `x[n−1]`、`y[n−1]=0`**，避免直流阶跃造成瞬态。
- 宽带能量：`rms = sqrt(sum(y²)/160)`；`level_dbfs = 20·log10(max(rms, 1e-6))`（下限 −120 dBFS）。同时记录帧内 `peak = max|y|`。
- 鼾声频带：对 y 使用二阶 Butterworth 高通 80 Hz + 二阶低通 1500 Hz（系数以 RBJ cookbook/双线性变换生成，并用数值测试验证：转折频率处约 −3 dB ±0.5 dB，40 Hz 处比通带低 ≥ 11 dB）；状态跨帧保持，重置时清零。每帧计算 `band_rms_dbfs`（同样下限 −120）和 `band_ratio = band_energy / (wideband_energy + ε)`，能量取平方和，`ε = 1e-10`。
- **噪声底（宽带与带内各一条）**：对 `level_dbfs` 和 `band_rms_dbfs` 各维护一个直方图，用于分别得到 `noise_floor_dbfs` 与 `band_noise_floor_dbfs`。（原先用宽带噪声底去比较带内电平，两者量纲不一致，故分开。）
  - 分桶：100 个 1 dB 桶覆盖 −100…0 dBFS，桶 `i` 为 `[−100+i, −99+i)`；范围外 clamp 到首/末桶。
  - 窗口：环形缓冲保存最近 `noise_floor_window_seconds`（默认 30 s = 3000 帧）每帧的桶索引（u8），入桶/出桶同步更新计数。
  - 估计：每 100 ms（10 帧）取第 `noise_floor_percentile`（默认 20）百分位：累计计数达到 `ceil(p × N)` 的最小桶，取桶中心 `−100 + i + 0.5`。
  - **预热**：窗口内有效帧 < `noise_floor_min_seconds`（默认 10 s = 1000 帧）时噪声底视为未知，检测状态为 `warming_up`，不产生命中帧；直方图仍照常累积。启动、重置后长 gap（> `noise_floor_reset_gap_seconds`，默认 60 s）、格式变化后均重新预热；短 gap 保留直方图。
  - **冻结**：处于 CANDIDATE 状态时暂停入桶和出桶（窗口不前进）。为避免持续噪声（风扇开启、长时间说话）使噪声底永远不更新，连续冻结超过 `noise_freeze_max_seconds`（默认 30 s）后强制解冻，噪声底随之上升，事件会因不再满足 active 而自然结束。onset 判定前的 3–5 帧会进入直方图，对 20% 分位影响可忽略，属已接受的近似。
- 数字静音与削波：连续 ≥ 1 s 的帧 `peak==0`（麦克风静音/掉线导致）置 `signal_state=silent`，状态页告警，帧仍正常处理但因 `absolute_floor_dbfs` 不会产生事件。削波在 dispatcher 对**原始 S16**统计（`|sample| ≥ 32767`），1 s 内削波样本 > 1% 记录日志并计入状态 `clipped_samples_total`（提示增益过高）；削波统计只用于告警，不参与规则。
- 初版不做 FFT，不将节律自相关作为硬条件。可记录 100 Hz 帧能量包络供后续离线分析。这样减少算法复杂度和不确定误报来源。

### 5.3 候选状态机及参数

默认参数（均可配置，现场调试后再定；与第 11 节配置键一一对应）：

```text
sample_rate=16000
frame_ms=10
band_low_hz=80
band_high_hz=1500
absolute_floor_dbfs=-50
noise_margin_db=8
band_margin_db=6
band_ratio_min=0.35
attack_window_frames=5   # 最近 5 帧中至少 attack_min_hits 帧命中则进入候选
attack_min_hits=3
hangover_frames=50       # 连续 500 ms 无命中后关闭候选
min_event_ms=200
merge_gap_ms=800
max_event_seconds=15
noise_floor_window_seconds=30
noise_floor_percentile=20
noise_floor_min_seconds=10
noise_freeze_max_seconds=30
filter_warmup_ms=100
```

逐帧判定（仅在非 `warming_up`、非滤波预热期时执行）：

- `active_threshold_dbfs = max(absolute_floor_dbfs, noise_floor_dbfs + noise_margin_db)`；`active = level_dbfs > active_threshold_dbfs`。
- `snore_like = band_ratio >= band_ratio_min && band_rms_dbfs >= band_noise_floor_dbfs + band_margin_db`。
- **命中帧** `hit = active && snore_like`。
- 滤波预热：每次重置后头 `filter_warmup_ms`（100 ms = 10 帧）的帧不参与命中（IIR/重采样瞬态）。

状态机：

```text
IDLE ──(最近5帧内≥3命中)──▶ CANDIDATE ──(连续50帧无命中)──▶ PENDING ──(距终点>800ms无新onset)──▶ 判定并落库
                                ▲                              │
                                └──(800ms内新 onset，且合并后≤max)┘
```

- onset：`attack_window_frames` 窗内命中数 ≥ `attack_min_hits`；事件起点回溯到该窗内首个命中帧。
- CANDIDATE：任何命中帧重置 hangover 计数；累计 `hit` 帧的统计量。
- 关闭：连续 `hangover_frames` 帧无命中时，终点 = 最后一个命中帧的结束位置，进入 PENDING。
- PENDING：保留 `merge_gap_ms`（自终点起算）等待合并；该期间出现新 onset 且 `新起点 − 终点 ≤ merge_gap_ms` 且合并后跨度 ≤ `max_event_seconds` 时合并（统计量累加）；否则先落库 pending，再开始新候选。注意事件因此在最后命中后约 800 ms 才落库，状态页的“最近事件”会有此延迟。
- 最短时长在**合并后**判定：`< min_event_ms` 的事件丢弃并计数 `events_discarded_short`。
- 最大时长：CANDIDATE 跨度达到 `max_event_seconds` 时，在该处落库并立即在下一帧以 `continued=1` 重开。
- 强制关闭（不等 hangover/合并，直接以最后命中帧为终点判定并落库，写入 `end_reason`）：`segment_end` / `gap` / `detector_gap` / `shutdown`。其余情况 `end_reason` 为 `normal` 或 `max_duration`。
- 事件特征：`peak_dbfs`（事件内 peak 最大值）、`mean_level_dbfs`、`mean_band_ratio`（后两者对命中帧求平均）、`noise_floor_dbfs`（onset 时宽带噪声底）。
- **`rule_score`**（0–1，非概率）：

  ```text
  level_term = clamp(mean(level_dbfs − active_threshold_dbfs) / 20 dB, 0, 1)   # 命中帧均值
  ratio_term = clamp((mean_band_ratio − band_ratio_min) / (0.9 − band_ratio_min), 0, 1)
  rule_score = 0.6·level_term + 0.4·ratio_term
  ```

  仅当上述任一输入非有限值（NaN/Inf）或命中帧数为 0 时为 `null`，API 原样返回 null，不用 0 代替。20 dB 与 0.9 为初始归一化常数，随 `detector_version` 记录。
- 时间边界由已存储帧序列与 5.1 的映射产生，UTC 由 4.3 的公式换算。
- UI 审核标注 `snore / not_snore / unreviewed` 仅用于人工校验，不自动更新规则。

### 5.4 不连续与重置

检测器收到任何 `Gap`、`SegmentStart`（格式变化）、检测队列丢块（`detector_drop`）或被 watchdog 重启时：

1. 若有 CANDIDATE/PENDING：以最后命中帧为终点强制落库，`end_reason` 对应 `gap`/`detector_gap`；少于 `min_event_ms` 的按规则丢弃。
2. 清空 attack 窗口、hangover 计数。
3. 重置重采样器、DC blocker 与 biquad 状态（DC blocker 按 5.2 初始化），并以 gap 后首个输入帧建立新的 `anchor_in_offset`。
4. 开始 `filter_warmup_ms` 预热；噪声直方图按 5.2 规则保留（短 gap）或清空（长 gap/格式变化）。
5. 帧环形缓冲中不足一帧的残留样本丢弃（不跨 gap 拼帧）。

正常的段轮转（无 gap、格式不变）**不重置** DSP 状态，只关闭当前候选；下一段在连续音频上继续，因此噪声底和滤波状态连续。

### 5.5 算法验证

Rust 单元测试使用确定性 PCM fixture：静音、正弦音、不同频带噪声、振幅阶跃、短脉冲、周期振幅调制音、风扇/说话录音。验证项：

- RMS/dBFS、滤波响应（转折频率/阻带）、噪声基线（含预热、冻结、解冻、范围外 clamp）、attack/hangover、合并（含 500–800 ms 窗口边界）、`max_event_seconds` 切分与 `continued`、最短时长在合并后判定。
- 重采样：各采集率（16/32/44.1/48/96 kHz）输出长度与比例精确；跨块连续性（同一信号不同分块输出一致）；折返频率抑制 ≥ 60 dB；通带波动；`resampler_delay` 补偿后，已知起点脉冲的事件偏移误差 ≤ 1 帧（10 ms）。
- 帧→偏移映射：非 16 kHz 采集下，合成已知起止的信号，事件 `start/end_offset_frames` 与真值误差 ≤ 10 ms（换算成帧数）。
- 不连续：注入 xrun/溢出 gap，验证事件按 `end_reason=gap` 关闭、检测器重置、gap 后无跨 gap 事件、`lost_frames` 与 `wall(o)` 映射正确；`detector_drop` 同理。
- 分段：按已存储帧数轮转；`SegmentStart` 先于事件入库；事件不跨段；壁钟阶跃触发轮转并正确修正 `unsynced` 段。
- 数字静音不产生事件并触发 `signal_state=silent`。

合成 fixture 只能证明实现符合规格，不能证明识别准确率。

真实试用：人工给录音事件加标签，逐晚统计 TP/FP/FN、precision/recall；保存误报/漏报片段的时间戳以便调参数。至少先收集多晚样本，任何准确率结论必须附样本数和环境。

## 6. 存储与自动回收

- `recording_max_bytes` 默认 20 GiB；`recording_cleanup_target_bytes` 默认 19 GiB；`recording_reserve_bytes` 默认 512 MiB，配置要求 target < max。
- 文件大小仅统计 recordings 根目录下 WAV/partial 录音；DB/日志独立，不计入音频配额。实际文件系统 free space 另行检查。
- 每片段完成后、进程启动恢复扫描时检查总量。若 >max，按开始时间升序删除 complete 文件，直到 <=target。当前写入片段和 partial 不由 retention 删除。
- 删除前校验规范化路径仍在 recordings 根目录下，DB 状态和文件 id 匹配。删除成功才将 segment 标记 deleted；事件元数据/人工标记继续保留，音频 API 对删除录音返回 410，Portal 显示“录音已过期”。
- 删除失败则日志记录并尝试下一个最旧的 complete；仍超限时 `storage_pressure=true`。free space 低于 reserve 且清理无效时停止录音并明确告警。

## 7. 数据模型（SQLite）

WAL、外键、busy timeout，schema 版本用 `PRAGMA user_version`。DB 操作不在音频采集线程执行。

`recording_segments`：

- `id TEXT PRIMARY KEY`
- `relative_path TEXT NOT NULL UNIQUE`
- `started_at_utc TEXT NOT NULL`, `ended_at_utc TEXT`
- `capture_rate_hz INTEGER NOT NULL`, `channels INTEGER NOT NULL`, `sample_format TEXT NOT NULL`
- `channel_mapping TEXT NOT NULL`, `frame_count INTEGER NOT NULL DEFAULT 0`, `size_bytes INTEGER NOT NULL DEFAULT 0`
- `xrun_count INTEGER NOT NULL DEFAULT 0`, `gap_count INTEGER NOT NULL DEFAULT 0`, `lost_frames_total INTEGER NOT NULL DEFAULT 0`
- `time_quality TEXT NOT NULL`：synced / unsynced / corrected；`boot_id TEXT NOT NULL`；`clock_drift_ppm REAL`
- `end_reason TEXT`：duration / gap / time_step / format_change / size_limit / shutdown / error
- `status TEXT NOT NULL`：recording / complete / interrupted / deleted / missing
- `created_at_utc TEXT NOT NULL`

`audio_gaps`（缺口登记，由 dispatcher 经 DB writer 写入）：

- `id INTEGER PRIMARY KEY`
- `segment_id TEXT NOT NULL REFERENCES recording_segments(id)`
- `offset_frames INTEGER NOT NULL`（缺口发生在该已存帧偏移之前）
- `lost_frames INTEGER NOT NULL`, `kind TEXT NOT NULL`（xrun / stream_error / capture_ring_overflow / queue_overflow / stream_rebuild）
- `estimate_source TEXT NOT NULL`：timestamp / counter / unknown
- `at_utc TEXT NOT NULL`

写入采用批量、异步、不在采集线程执行；gap 先在内存中登记、段关闭时一并落盘，异常退出可能丢失最后几秒的 gap 记录（已知限制，`interrupted` 段的 gap 信息可能不完整，Portal 标注）。

`snore_events`：

- `id TEXT PRIMARY KEY`
- `segment_id TEXT NOT NULL REFERENCES recording_segments(id)`
- `start_offset_frames INTEGER NOT NULL`, `end_offset_frames INTEGER NOT NULL`
- `started_at_utc TEXT NOT NULL`, `ended_at_utc TEXT NOT NULL`, `duration_ms INTEGER NOT NULL`
- `rule_score REAL`, `detector_version TEXT NOT NULL`
- `peak_dbfs REAL`, `mean_level_dbfs REAL`, `mean_band_ratio REAL`, `noise_floor_dbfs REAL`
- `end_reason TEXT NOT NULL DEFAULT 'normal'`：normal / max_duration / segment_end / gap / detector_gap / shutdown
- `review_status TEXT NOT NULL DEFAULT 'unreviewed'`
- `continued INTEGER NOT NULL DEFAULT 0`, `created_at_utc TEXT NOT NULL`

事件定义：`start_offset_frames/end_offset_frames` 为该段**已存帧**偏移（半开区间 `[start, end)`），`started_at_utc/ended_at_utc` 按 4.3 的 `wall(o)` 公式计算，因此事件跨 gap 时不存在（检测器在 gap 处强制关闭）。事件 `duration_ms = (end_offset_frames − start_offset_frames) × 1000 / capture_rate_hz`（四舍五入）。事件落库前必须已存在对应 segment 行，否则进入重试队列，不得丢弃。

索引：`snore_events(started_at_utc)`、`snore_events(segment_id,start_offset_frames)`、`recording_segments(started_at_utc)`、`audio_gaps(segment_id,offset_frames)`。

## 8. HTTP API

前缀 `/api/v1`，错误统一 `{"error":{"code":"...","message":"..."}}`。

| Method | Endpoint | 说明 |
|---|---|---|
| GET | `/api/v1/status` | 服务/录音/麦克风/检测/存储/xrun/队列状态 |
| GET | `/api/v1/days?from=YYYY-MM-DD&to=YYYY-MM-DD` | 有数据日期和每日概况 |
| GET | `/api/v1/segments?date=YYYY-MM-DD` | 当天录音段、覆盖区间、状态、格式、时长、`time_quality`、`lost_frames_total`、`end_reason` |
| GET | `/api/v1/segments/{id}/gaps` | 该段 gap 列表（`offset_frames`、`lost_frames`、`kind`、`estimate_source`），供时间轴和播放头映射 |
| GET | `/api/v1/events?date=YYYY-MM-DD` | 当天候选事件、起止、offset、rule_score、review_status |
| GET | `/api/v1/events/{id}` | 单事件详情和对应 segment |
| PATCH | `/api/v1/events/{id}` | body `{ "review_status":"snore"|"not_snore"|"unreviewed" }` |
| GET | `/api/v1/segments/{id}/audio` | `audio/wav`，支持 `Range`、`206`、`Content-Range`、`Accept-Ranges` |
| GET | `/` | Portal |

- 音频按 range 固定缓冲读取，不整体加载到 RAM；错误码：不存在 404、已删除 410、未完成 409、无效 Range 416。
- 音频 Range 细节：支持 `bytes=a-b`、`bytes=a-`、`bytes=-n`；多段 Range 不支持，直接返回完整 200（按 RFC 允许）；所有响应带 `Accept-Ranges: bytes`、`Content-Length`，支持 `HEAD`。`recording` 状态的 `.partial` 不提供播放（409）。`complete`/`interrupted` 段文件定长不再变化，可安全返回 `ETag`（如 `segment_id-size_bytes`）。响应体总字节数以 DB `size_bytes` 与实际文件大小一致为前提，不一致则标记 `missing`/告警。
- 事件和段的 JSON 对外时间均为 UTC RFC 3339，并给出 `offset_frames` 和 `capture_rate_hz`；客户端不得用 `started_at_utc` 差值推算播放位置（gap 会造成偏差），必须用 offset。
- 参数严格校验、SQL 参数绑定、文件路径不得由请求拼接。只允许同源 CORS。
- MVP 无认证，限可信局域网，不做公网端口映射；HTTP API 不提供 start/stop 控制接口。

## 9. Portal UI

- 顶栏实时显示服务、麦克风、录音、检测状态及磁盘用量/上限。
- 日期导航显示有录音的夜晚；主时间轴按本地时间绘制录音覆盖段和鼾声候选色块。
- 事件列表显示开始/结束/时长/规则分数/人工标签；点击候选自动选择录音段、加载播放器并 seek 到 `start_offset_frames / capture_rate_hz`（建议提前约 2 s 上下文，不小于 0）。
- 时间轴绘制规则：用 `gaps` 把已存帧偏移映射到墙钟时间，gap 显示为带时长的断点；`time_quality=unsynced` 的段用虚线或标注“时间未校准”；`interrupted` 段标注 gap 信息可能不完整。播放头与段内 gap 相关的映射用同一张 gap 表，不用时间差值。Portal 不显示检测器盲区（见 4.3 已知限制）。
- HTML `<audio controls>` 使用 Range seek；时间轴显示播放头，播放器 `timeupdate` 更新当前位置。事件在小时片段边缘时选中正确片段。
- 点击“鼾声/误报”调用 PATCH；失败时保留当前显示并提示重试。已删除片段的事件保留但显示“录音已过期”，不可播放。
- 加载、空日期、无设备、录音中断、磁盘告警、服务断开都有明确状态；不显示假数据、不自动播放。
- 原生 HTML/CSS/JS，无 CDN/构建链；适配桌面和手机窄屏。音频不离开局域网、不上传云端。

## 10. 工程实现

### 依赖与编译

- Rust stable，Cargo；Pi ARMv7/ARM64 target 以实际 OS image 确认，发布采用对应 target triple；`opt-level="z"` 或 `3` 通过 benchmark/RSS 实测选择，strip binary。避免 `target-cpu=native` 产物部署到不同 Pi。
- `cpal`：ALSA audio capture；Linux 构建仍需 ALSA development files，运行时依赖 ALSA 系统库。[1]
- `dasp` provides modular audio fundamentals and documents no dynamic allocations/no dependencies; only selected components are needed.[2]
- `biquad` offers first/second order IIR filters, including DF1 and DF2T, and is `no_std`.[4]
- `rubato` is a chunk-based resampler; its preallocated-buffer API is intended for real-time use without allocations/blocking in processing.[3]
- `axum` + Tokio：HTTP/API、Portal 静态文件；Axum 是基于 Tokio 的 Rust HTTP routing/request-handling library。[7] 音频 endpoint 自行实现 Range/seek。
- `rusqlite` + SQLite：优先系统 SQLite 动态链接以缩小 binary；是否 bundled 按 ARM 部署需求评估。WAL。
- TOML/JSON/logging crate 只启用所需 features；Cargo.lock 锁定版本。
- 测试用 `cargo test`；在 Pi target 实际编译和运行 DSP fixture。
- 可选分类器研究：Rust `soundevents` 提供 CED AudioSet ONNX 声音事件分类接口，tiny 模型约 6.4 MB，输入为 16 kHz mono PCM；它是通用声音事件分类，不等同于经验证的鼾声检测器。非 MVP，只有规则检测误报/漏报明显且完成 Pi 性能测试后才考虑。[6]
- Portal 静态文件直接从安装目录读取。

### 目录

```text
2610-snore-monitor-pi/
├── TECH_SPEC.md
├── README.md
├── Cargo.toml
├── Cargo.lock
├── config.example.toml
├── systemd/snore-monitor.service
├── src/
│   ├── main.rs
│   ├── audio_capture.rs
│   ├── dispatcher.rs
│   ├── timeline.rs
│   ├── resampler.rs
│   ├── recorder.rs
│   ├── detector.rs
│   ├── storage.rs
│   ├── retention.rs
│   ├── http_server.rs
│   └── bounded_queue.rs
├── web/{index.html,app.css,app.js}
└── tests/ (Rust unit/integration tests)
```

### systemd 与权限

以专用 `snore-monitor` 用户运行，不用 root。`Restart=on-failure`，配置 `UMask=0027`，写权限只给配置的 DB/录音目录；stdout/stderr 到 journald。启动参数明确指定 config。系统升级后服务需通过依赖检查和长时间录音 smoke test。

## 11. 配置示例

路径默认 `/etc/snore-monitor/config.toml`，环境变量 `SNORE_MONITOR_CONFIG` 可覆盖开发配置；启动时一次性校验，变更需重启。

```toml
[server]
bind_address = "0.0.0.0"
port = 8080
http_threads = 2

[audio]
alsa_device = ""             # 唯一 USB capture 设备可自动选；多设备必须明确指定
preferred_rate_hz = 16000
channels = 1
format = "S16_LE"
period_ms = 10
buffer_ms = 200
segment_duration_seconds = 3600     # 按已存帧数轮转，不对齐整点
mono_channel_index = 0
capture_ring_ms = 500
recording_queue_ms = 2000
detection_queue_ms = 1000
gap_tolerance_ms = 20              # 相邻块时间戳不连续超过此值才登记 gap
gap_rotate_seconds = 5             # 单次 gap 超过此值强制轮转分段
header_flush_interval_seconds = 30
shutdown_timeout_seconds = 10

[detector]
detection_rate_hz = 16000
frame_ms = 10
band_low_hz = 80
band_high_hz = 1500
absolute_floor_dbfs = -50.0
noise_margin_db = 8.0
band_margin_db = 6.0
band_ratio_min = 0.35
noise_floor_window_seconds = 30
noise_floor_percentile = 20
noise_floor_min_seconds = 10
noise_floor_reset_gap_seconds = 60
noise_freeze_max_seconds = 30
filter_warmup_ms = 100
attack_window_frames = 5
attack_min_hits = 3
hangover_frames = 50
min_event_ms = 200
merge_gap_ms = 800
max_event_seconds = 15

[storage]
recording_dir = "/mnt/snore-monitor/recordings"
database_path = "/var/lib/snore-monitor/app.db"
recording_max_bytes = 21474836480
recording_cleanup_target_bytes = 20401094656
recording_reserve_bytes = 536870912
```

配置校验：所有路径绝对路径；存储 target < max；period/buffer 正值；检测频带在 Nyquist（8 kHz）以下且 `band_low_hz < band_high_hz`；HTTP threads 1–8；`detection_rate_hz` 必须为 16000；`mono_channel_index < channels`；`channels` 为 1–2；`attack_min_hits ≤ attack_window_frames`，`hangover_frames ≥ 1`，`max_event_seconds * 1000 > min_event_ms`，`noise_floor_min_seconds ≤ noise_floor_window_seconds`；`noise_floor_percentile` ∈ [1, 50]；队列/ring 不小于 2 个 period；用户配置采样率必须在 5.1 支持表内且由设备测试支持，否则按 documented fallback 或失败，不能静默改配置。`segment_duration_seconds` 必须使单段大小 < 4 GiB（`rate × channels × 2 × seconds`）。

## 12. 健康状态和日志

`/api/v1/status` 返回真实运行值，至少含 service、recording、microphone、detector、capture format、detection rate、used/max/free bytes、storage pressure、ALSA xrun/gap、`lost_frames_total`、`capture_ring_overflow_frames`、dropped detection blocks、`detector_state`（warming_up / idle / candidate / pending）、`noise_floor_dbfs` / `band_noise_floor_dbfs`、`signal_state`（ok / silent）、`clipped_samples_total`、`time_quality`/`time_synced`、`gap_detection_limited`、`clock_drift_ppm`、`event_fk_retries`、`events_discarded_short`、recorder/capture/detection queue high-water、各队列控制槽占用、DSP processing time（avg/p99/max）、last_error、now_utc。任何状态未知返回 unknown/degraded，不能填充假 active。

日志记录设备协商、启动/停止、片段完成、xrun/gap、队列溢出、DSP 时间统计、回收动作和错误；禁止写入音频波形或隐私内容。

## 13. 验收测试

### 自动测试

- Rust 单元测试：已知幅度 RMS/dBFS；biquad 频率响应；重采样混叠抑制与跨 block 连续性；噪声底直方图；attack/release 状态机和事件合并。详细用例清单见 5.5（重采样各采样率、帧→偏移映射、gap/重置、分段协调、数字静音）。
- Gap 与时间线：注入 xrun/队列溢出，验证 `frame_count`、`audio_gaps`、`wall(o)` 映射、段轮转原因；壁钟阶跃触发轮转与 `unsynced` 修正；控制消息（SegmentStart/End/Gap）在队列压力下不丢。
- WAV：header/endian/rate/channel/帧数正确；多次写入后可由标准播放器打开；4 GiB 轮转边界。
- 并发/故障：队列满、ALSA xrun、磁盘满、SIGTERM；采集不得被 HTTP 长请求阻塞，错误能可见。
- retention：按旧到新回收，仅删 complete；active/partial 不删；文件删除和 DB 状态一致；路径穿越拒绝。
- API：JSON、Range 206/416、404/409/410、审标签校验、日期边界。
- Portal：日期加载、点击候选跳转、播放头同步、误报审核、过期事件状态。

### Raspberry Pi 3B 实测门槛

1. 在目标板编译安装；列出麦克风实际 ALSA formats/rates/channels，验证协商/回退真实可用。
2. 连续运行至少 8 小时，录音+检测+HTTP 同时启用；核查 WAV 播放、文件长度、xrun/gap、CPU/RSS/温度、磁盘写入。目标 RSS <80 MiB、DSP 平均 CPU <10% 单核；若超出先 profile 再调整。
3. 用真实录音人工标签评估误报/漏报，报告样本量，不用合成数据推导识别准确率。
4. 测 Portal 手机浏览器回放、拖动、候选定位和小时文件切换。
5. 低配额强制触发 retention，确认只删最旧完整片段，事件元数据留存并显示过期。
6. 拔麦克风、模拟 USB/磁盘故障、kill/reboot，验证状态显示与恢复。
7. 时间线精度：发出已知节拍声（如手机播放整点满足的脉冲音）连续一小时以上，比较事件偏移与真值、`clock_drift_ppm`；在采集流上注入人为 CPU 压力（如 `stress-ng`）并检查 xrun/gap 统计、回放对齐和 gap 显示。
8. 在目标板验证 CPAL/ALSA 是否提供可靠的捕获时间戳（即 4.3 的 gap 检测能否使用 `estimate_source=timestamp`）；不可用时确认 `gap_detection_limited` 已在状态页显示。
9. NTP 未同步启动（断网开机）：验证 `unsynced` 标注、同步后壁钟阶跃触发轮转与修正。

## 14. 尚待目标设备确认

- USB 麦克风具体型号及 ALSA 实际支持格式/采样率/通道。
- Pi 的 OS 架构 (armv7/aarch64)、CPAL/ALSA 实际设备配置与 Rust target triple；Cargo crate feature/versions 锁定后验证。
- 麦克风到床头的摆放距离、增益、卧室噪声以及按样本校准后的带通/阈值。
- CPAL/ALSA 在该 Pi 上是否给出可靠的捕获时间戳、实际 period/buffer 和 xrun 行为；该麦克风的时钟漂移（ppm）。
- 重采样 crate 的具体 API、群延迟、输出长度与锁定版本，以及其满足 5.1 中混叠抑制和通带指标的实测结果。
- 当前默认阈值、噪声底窗口和 `rule_score` 归一化常数（20 dB、0.9）均为初值，需用真实夜间录音校准。
- 实际 USB SSD 文件系统和用户要求的存储上限/保留天数。

## Sources

[1] https://github.com/RustAudio/cpal — CPAL Rust audio I/O
[2] https://github.com/RustAudio/dasp — DASP Rust DSP building blocks
[3] https://docs.rs/rubato/latest/rubato — Rubato Rust resampling docs
[4] https://github.com/korken89/biquad-rs — Rust biquad IIR filter
[6] https://github.com/findit-ai/soundevents — soundevents CED sound classifier
[7] https://github.com/tokio-rs/axum — Axum Rust HTTP framework
