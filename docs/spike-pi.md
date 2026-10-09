# Pi 硬件与库可行性 spike — T0.2（占位 / 实物待测）

依据 `TECH_SPEC.md` §14、`tasks.md` T0.2。本文档不记录任何虚数据；每项
预留“结果：待实测”的空位，待 Pi 3B + USB 麦克风上跑完实测后填入。

实测前请记录：Pi 型号与内存、Raspberry Pi OS 版本与架构（armv7 / aarch64）、
内核版本、ALSA lib / alsa-utils 版本、USB 麦克风型号与固件 ID、Rust toolchain
与 target triple。

---

## 1. 麦克风实际 ALSA 能力

**需要测什么**：

- `arecord -l` 看到的 card / device 编号、卡名、USB ID；
- `cat /proc/asound/cardN/stream0` 列出的所有 PCM formats、rates、bit depths；
- 是否同时支持 16/32/44.1/48/96 kHz，至少需要 §5.1 表里的 16/32/44.1/48/96 五个
  采集率之一被设备原生支持。
- mono / stereo 是否可选；是否需要切到特定 subdevice / channel 才能获得 mono。

**怎么测**：

```sh
arecord -l
for c in /proc/asound/card*/stream0; do echo "=== $c ==="; cat $c; done
arecord -D plughw:1,0 --dump-hw-params -f S16_LE -r 48000 -c 1 -d 1 /dev/null
```

**判据**：

- 麦克风能提供至少一种 §5.1 支持的采样率与 S16_LE；若只支持其他格式（例如
  S24_3LE、S32_LE），需要记录并决策是否要加额外降采样或换麦克风。
- device name（`plughw:N,M` 或 `hw:N,M`）可在 `audio.alsa_device` 里直接使用。

**结果：待实测**。

## 2. CPAL `supported_input_configs()`

**需要测什么**：

- CPAL 在该 Pi + 该麦克风上返回的 `supported_input_configs()` 列表与
  sample format、min/max sample rate、channels、sample format；
- CPAL 实际创建的 stream 使用的 period size / buffer size（与 `period_ms` /
  `buffer_ms` 配置可能不完全一致）。

**怎么测**：

写一个最小 Rust 二进制（可临时加在 `src/bin/spike-cpal.rs` 或独立 crate），调用：

```rust
let host = cpal::default_host();
let device = host.default_input_device().expect("no input");
let configs = device.supported_input_configs().unwrap();
for c in configs { println!("{:?}", c); }
let actual = device.default_input_config().unwrap();
println!("default = {:?}", actual);
```

**判据**：

- CPAL 能枚举到全部候选配置；默认配置与 `audio.preferred_rate_hz` 协商结果
  一致。
- 实际 period / buffer 记录在启动日志中（参考 §4.1 要求）。

**结果：待实测**。

## 3. CPAL 捕获时间戳是否可靠

**需要测什么**：

- CPAL 在该 backend/平台上能否提供每块 PCM 的捕获时间戳（`StreamTimestamp`）。
- 时间戳是单调时钟、UTC、还是其他。
- 连续运行至少 10 分钟，采样相邻块时间戳差值与期望值（`period_ms`）的偏差
  是否控制在 `< gap_tolerance_ms` (20 ms) 内。

**怎么测**：

- 在 callback 里取 `cpal::StreamTimestamp::Instant<...>` 并保存连续 N 块的差值；
- 同时记录 `CLOCK_MONOTONIC` 作交叉参考。
- 拔插麦克风后时间戳是否跳变。

**判据**：

- 若时间戳**连续**且与 `CLOCK_MONOTONIC` 偏差 < 1 ms/块，§4.3 的 gap 检测可使用
  `estimate_source='timestamp'`；否则只能依赖 `counter`/`'unknown'`，状态页需显示
  `gap_detection_limited=true`。

**结果：待实测**。 

**对 `estimate_source` 的影响（需根据本项结果填）**：

- 时间戳可靠 → `estimate_source='timestamp'`，gap 估算用相邻块差分。
- 不可靠（持续偏差 > `gap_tolerance_ms`，或仅能在重建后用） → `estimate_source='counter'`/`'unknown'`，状态页 `gap_detection_limited=true`。

## 4. 拔出麦克风行为

**需要测什么**：

- 热拔出 USB 麦克风后，CPAL 的 error callback 是否被调用？多久？
- error 信息是否被 CPAL 转为 `StreamError`/`BackendSpecific`？
- 重插后自动恢复需要多久？是否能从退避起点 1 s 重连？
- 拔掉期间 `microphone` 状态是否正确置 `disconnected`（不影响 WAV 写入）。

**怎么测**：

- 运行服务并录音；
- `journalctl -u snore-monitor -f` 同时进行；
- `usb拔` 麦克风 30 秒、5 分钟、1 小时各一次；
- `usb插回`，检查 gap 类型、错误码、录音是否恢复。

**判据**：

- 拔掉后服务不崩溃、状态页提示 `disconnected`、恢复扫描能识别 partial。
- 重插后 30 s 内重建 stream 并继续录音；重插到下条 gap 之间丢失的 frame 数
  与重建耗时一致。

**结果：待实测**。

## 5. 麦克风时钟相对系统单调时钟的漂移

**需要测什么**：

- 麦克风采样率与系统 `CLOCK_MONOTONIC` 的相对漂移（ppm）。
- 至少连续采样 1 小时才能看到趋势。

**怎么测**：

- 让麦克风采集粉红噪声 / 静音 ≥ 1 小时；
- 每隔 100 ms 取一次 `initiating_at_utc` 与 wall time，计算
  `(frame_count/nominal_rate - monotonic_elapsed) / monotonic_elapsed × 1e6`；
- 记录与时间戳法（§3）的差异。

**判据**：

- 漂移数量级 < 100 ppm 为典型 USB 麦克风；高则需考虑使用 `alsa` crate 或
  改用 timestamp 法。
- 写入 `/api/v1/status` 中的 `clock_drift_ppm`。

**结果：待实测**。

## 6. 重采样实测（验证 §5.1 指标）

**需要测什么**：

- 自实现的 windowed-sinc 多相 FIR（Kaiser β=8，每相 64 抽头）在 16/32/44.1/48/96 kHz
  输入下的输出长度与比例是否精确；
- 通带 0–4 kHz 波动 ≤ ±0.5 dB；
- 14.5–15.9 kHz 输入折返到 100–1500 Hz 鼾声频带的衰减 ≥ 60 dB；
- 跨块连续性：同一信号不同分块输出一致；
- 群延迟 `resampler_delay_in_frames` 补偿后，已知起点脉冲的事件偏移误差
  ≤ 1 帧（10 ms）。

**怎么测**：

- 复用 `tests/resampler_metrics.rs`（已存在，见 `tasks.md` T2.8）；
- 对每个采集率构造已知正弦/扫频信号，记录输出、计 length 与能量；
- 折返：14.5–15.9 kHz 灌入 32/44.1/48/96 kHz，输出后 FFT 查看在 100–1500 Hz
  的能量衰减。

**判据**：

- 输出长度 = `floor(input_length × L/M)` 误差 < 1 sample；
- 通带 / 折返指标达标；
- 群延迟解析值与实测一致（差 ≤ 1 sample）。

**结果：待实测**。

## 7. CPAL 不满足需求时的退路

**需要测什么**：

- 若 §2 / §3 中 CPAL 不能稳定枚举或不能提供时间戳，决定是否退到 Rust
  `alsa` crate 直接调用 ALSA API。

**怎么测**：

- 试写一个最小 `alsa` crate 例子枚举 `pcm`、读 `hw_params`、设置 period / buffer；
- 对比与 CPAL 在同一麦克风上的能力差异。

**判据**：

- 若 CPAL 不满足任意一项必需能力，采 `alsa` crate；DSP/检测/存储代码不变。

**结果：待实测**。

---

## 总结（待填）

- 麦克风型号、OS 版本、Rust target triple：待实测。
- 是否需要从 CPAL 退到 `alsa`：待实测。
- 锁定 Cargo.toml 的 CPAL / alsa / dasp / biquad 版本：待实测。