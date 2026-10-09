# Portal 手工测试清单（T6.5）

本文件是阶段 6 的人工测试清单，不是自动测试。完成 T6.1–T6.4 后，
在桌面和手机浏览器上各走一遍，逐条勾选。表格里的"验证点"对应
`TECH_SPEC.md` §9 中的硬性要求与 `tasks.md` 中 T6.5 列出的六类操作。

资源：

- 入口：浏览器打开 `http://<pi>:<port>/`（默认 port 见 `config.example.toml`）。
- API：`/api/v1/status`、`/api/v1/days`、`/api/v1/segments`、`/api/v1/segments/{id}/gaps`、
  `/api/v1/events`、`/api/v1/events/{id}`、`/api/v1/segments/{id}/audio`，PATCH
  `/api/v1/events/{id}`。所有路径与 `src/http_server.rs` 的 router 一致。
- 时间轴数学：`web/app.js` 中的 `offsetToWall(segment, gaps, offsetFrames)` 和
  `wallToOffset(segment, gaps, wallTime)`，手算样例（含一个 gap：offset 16000、
  lost 8000、rate 16000）的往返一致性已在开发脚本里验证 PASS。

## 一、日期加载

- [ ] **桌面**：打开首页，**左栏"有录音的夜晚"** 显示近 90 天内所有有数据的日期（来自 `GET /api/v1/days`），无数据日期**不显示**。
- [ ] **桌面**：日期项右侧显示段数 / 事件数 / 丢帧总数（对应 `DayView` 的 `segment_count` / `event_count` / `lost_frames_total`）。
- [ ] **桌面**：90 天内全空时，左栏显示"近 90 天没有录音"。
- [ ] **桌面**：刷新 / 一下时，日期列表重新拉取，旧选择保留。
- [ ] **手机**：窄屏（< 720px）下，左栏移到顶部；点击某天后自动滚动到时间轴。
- [ ] **服务断开**：拔网线或停服务，刷新页面：左栏应显示"日期列表加载失败：…"；顶栏右上角显示"服务断开"，并在底部告警区显示"⚠ 服务断开，正在重试…"；状态轮询应每 5 s 继续重试，恢复后自动更新。
- [ ] **验证点**：没有真实日期时**不显示假日期**或空事件列表；HTTP 失败有明确错误。

## 二、点击候选跳转 + 播放头同步

- [ ] **桌面**：时间轴下方轨道显示当天所有候选事件色块（按各自 `started_at_utc` / `ended_at_utc` 落点）；点击一个事件：
  1. 右侧事件列表中对应行高亮；
  2. `<audio>` 元素加载对应 `audio_url`（`/api/v1/segments/{id}/audio`）；
  3. 播放器不自动出声（§9：不自动播放），等待用户按播放。
  4. 点击播放后，时间轴拖动进度条从初始位置开始；`timeupdate` 事件实时驱动播放头在时间轴上滑动。
- [ ] **桌面**：点击播放后，播放头当前位置由 **同一张 gap 表**（`/api/v1/segments/{id}/gaps`）做反向映射（`offsetToWall(seg, gaps, floor(currentTime*rate))`），不是 `started_at_utc` 差值。
- [ ] **桌面**：跳到播放头对应时刻后，色块的开始/结束时间应与 `<audio>` 当前时间大致吻合（误差 ≤ 1 个 frame）。
- [ ] **手机**：触摸事件色块可点击；时间轴 SVG 在手机宽度下不超出可视区。
- [ ] **跨午夜事件**：事件 `started_at_utc` 落在前一天（`/api/v1/events?date=` 已通过 `event detail` 拉跨段并补齐 `audio_url`）时点击仍能播放：
  - [ ] 打开跨午夜的日期时，先按天的 `/segments?date=` 拉数据；事件里引用不在该天段的 `segment_id` 时，会通过 `GET /api/v1/events/{event_id}` 拉补齐。
  - [ ] 跨段点击后能播，并且时间轴上不绘制出那段覆盖（避免重复绘制）。
- [ ] **验证点**：播放头总是出现在事件色块范围内；事件偏移跨越 gap 时播放头位置正确（手算样例：rate 16000、gap offset=16000、lost=8000 的段里 offset=32000 对应壁钟 2.5 s）。

## 三、误报审核（PATCH）

- [ ] **桌面**：每个事件行三段都有 "鼾声 / 误报 / 撤销" 三个按钮：
  - 初始 `review_status=unreviewed`：仅"撤销"禁用。
  - 切到 `snore` 后，"撤销"恢复可用；切到 `not_snore` 同理。
- [ ] **桌面**：点击后事件列表行 + 时间轴色块颜色**立即**反映新状态（待请求成功后正式刷新），按钮旁显示"保存中…"，不闪烁回退。
- [ ] **桌面**：服务端 PATCH 失败（例如把服务停掉 / 改请求体 `review_status` 为非法值会拿 400）时：
  1. 标签和按钮恢复原状（不闪烁）；
  2. 底部 Toast 显示"审核保存失败：…（请重试）"（`web/app.js` `showTransientToast`）；
  3. 4 秒后 Toast 淡出，但状态保留供重试。
- [ ] **验证点**：PATCH 仅接受 `snore / not_snore / unreviewed`（参见 `tests/http_api.rs::patch_rejects_anything_else`）；前端不会发送其他值；非法值由后端返回 400 + `{error:{code:"invalid_review_status", message}}`，前端在 Toast 里显示。

## 四、过期事件 + 录音中断 + 服务告警

- [ ] **过期事件**：后端 status 为 `deleted` 的段：事件仍显示在列表，但行变灰、加"录音已过期，无法播放"提示；点击不加载音频；时间轴上事件色块用虚线灰底。
- [ ] **录音进行中**：status 为 `recording` 的段：事件行显示"录音进行中，无法播放"；`/audio` 返回 409，前端不会发出 `<audio>` 请求。
- [ ] **录音中断**：status 为 `interrupted` 的段：覆盖轨道用**虚线**绘制（`.tl-seg-interrupted`），鼠标 hover 显示"异常中断，gap 信息可能不完整"。播放器仍可加载（`/audio` 返回 206）。
- [ ] **未校准**：time_quality 为 `unsynced` 的段：覆盖轨道用灰色虚线绘制（`.tl-seg-unsynced`），hover 显示"时间未校准"；不影响播放（已有 PCM）。
- [ ] **磁盘告警**：状态页 `storage.storage_pressure=true` 时，顶栏告警区显示"⚠ 磁盘空间告警 …"；状态面板"存储"行的"磁盘告警"显示"是"。
- [ ] **gap 检测降级**：`capture.gap_detection_limited=true` 时，顶栏告警区显示"⚠ … gap 检测降级"；状态面板"麦克风 / 录音"行末显示"gap 降级：是"。
- [ ] **静音**：`signal_state=silent` 时，顶栏告警区显示"⚠ … 信号静音"。
- [ ] **验证点**：所有 4 类状态（过期 / 录音中 / 中断 / 未校准）都在 UI 上有明确文字 + 颜色区分，无空白或猜测。

## 五、小时切换（事件在小时片段边缘时选对片段）

- [ ] **手动构造**：构造一个事件，它的 `start_offset_frames / capture_rate_hz` 接近 3600 s（一小时整），但 `started_at_utc` 比小时整点早 0.1 s（模拟 §5.1 的检测器在 10 ms 帧粒度下跨越小时边界）。
- [ ] 点击该事件：
  1. 选中的段（`audio_url`）的 `started_at_utc` 是事件之前的那一段；不是后一份。
  2. 跳到的 `audio.currentTime` = `start_offset_frames / capture_rate_hz - 2`（不小于 0）。
- [ ] **跨午夜事件**：事件 `started_at_utc` 落在 `00:00:00` ± 10 ms 时，点击后选对的段是前一晚的最后一个段（由 `event detail` 的 segment view 提供），播放器能播、时间轴播放头不出错。

## 六、桌面 + 手机响应式

### 桌面（>= 1100px 宽）

- [ ] 顶栏一行展示 service / recording / microphone / signal / time 五个 badge。
- [ ] 主区三列：左 200px 日期、中 1fr 时间轴、右 320px 事件列表。
- [ ] 状态面板在顶栏与主区之间，4~5 列网格。
- [ ] 时间轴 SVG 高度 160 px，刻度均匀。
- [ ] 滚动：日期 / 事件各自独立滚动；时间轴始终完整可见。

### 平板（720px – 1100px）

- [ ] 主区两列：左日期、中时间轴；事件列表移到下方整行。
- [ ] 状态面板自动折行。

### 手机（< 720px）

- [ ] 主区单列，依次为：日期 / 时间轴 + 播放器 / 事件列表。
- [ ] 顶栏在窄屏下换行，badge 自动折行。
- [ ] 时间轴 SVG 仍然清晰可读（labels 不溢出）。
- [ ] 触摸点击事件色块响应正确，没有 hover-only 行为。

## 七、其他

- [ ] **不自动播放**：从打开到选中事件再到播放器就绪，**整个过程没有任何声音**；必须用户主动按播放按钮才出声。
- [ ] **无 CDN / 无外部资源**：浏览器 DevTools 的 Network 面板只应看到同源的 `index.html`、`app.css`、`app.js`，加上 `/api/v1/*` 请求与 `<audio>` 媒体请求；不得出现 fonts.googleapis、cdn.jsdelivr、analytics 等。
- [ ] **离线友好**：拔掉 Pi 后只显示"服务断开"；**没有事件列表占位**、没有"录音时长: 0:00"之类的假数据。
- [ ] **没有 PII 暴露**：URL 不包含鉴权 token；所有数据走 `/api/v1/*` 同源。

## 八、复现需要的辅助脚本

下面是检查 gap 映射往返一致性的开发脚本（不是运行时测试），用 Node 直接跑：

```bash
node -e '
function sortedGaps(g){return g.slice().sort((a,b)=>a.offset_frames-b.offset_frames);}
function offsetToWall(s,g,o){
  let l=0; for(const x of sortedGaps(g)){if(x.offset_frames<=o)l+=x.lost_frames;else break;}
  return (new Date(s.started_at_utc).getTime() + (o+l)/s.capture_rate_hz*1000);
}
const s={started_at_utc:"2025-01-01T00:00:00Z",capture_rate_hz:16000};
const g=[{offset_frames:16000,lost_frames:8000}];
console.log("offset 32000 ->", new Date(offsetToWall(s,g,32000)).toISOString()); // 期望 00:00:02.500
'
```

确认输出末尾是 `00:00:02.500Z` 即代表手算样例通过。