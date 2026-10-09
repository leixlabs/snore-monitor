# Portal — API contract assumed by `web/`

The Portal is plain HTML/CSS/JS served by the snore-monitor HTTP server. The
files in this directory consume the API below **exactly as documented here**.
This file is the contract the Portal implements today; cross-check it against
the server before treating any behaviour as final.

If the server contract diverges, the Portal must be updated, not the API
silently worked around — that is a hard rule from §9.

## Conventions

- All endpoints are under `/api/v1`.
- All JSON timestamps are RFC 3339 UTC strings (`YYYY-MM-DDTHH:MM:SS[.fff]Z`).
- All `date` query parameters are local dates (`YYYY-MM-DD`) on the server's
  configured timezone. The Portal does **not** do timezone conversion for
  range selection; it lets the server decide which segments fall under a date.
- Errors have shape `{"error":{"code":"<string>","message":"<string>"}}`.
- The Portal never sends secrets, never uploads audio, and never plays audio
  without an explicit user click (§9).

## Endpoints used

### `GET /api/v1/status`

Returns the full §12 status object. The Portal polls this on a 5 s interval
and on focus/visibility change. A failed poll flips the top-bar to
"服务断开".

Selected fields the Portal depends on:

| Path | Type | Use |
|---|---|---|
| `service.state` | `"ok" \| "degraded"` | top-bar pill |
| `recording.state` | string | recording pill |
| `recording.current_segment_id` | string \| null | optional live context |
| `microphone.state` | `"connected" \| "disconnected" \| "unknown"` | mic pill |
| `microphone.device` | string \| null | mic label |
| `detector.state` | string | detector pill |
| `detector.events_total` | integer | detector pill |
| `storage.used_bytes` | integer | disk bar numerator |
| `storage.max_bytes` | integer | disk bar denominator |
| `storage.pressure` | string | disk pressure pill |
| `storage.recording_stopped_low_disk` | bool | low-disk alert |
| `capture.gap_detection_limited` | bool | "gap 检测降级" caption (informational) |
| `capture.lost_frames_total` | integer | capture pill |

The Portal does not display fields it doesn't understand; missing fields just
render the corresponding pill as "未知".

### `GET /api/v1/days?from=YYYY-MM-DD&to=YYYY-MM-DD`

Returns `{"days":[{date, segment_count, event_count, duration_seconds,
  lost_frames_total}, …]}`. The Portal requests a 60-day window around today
on first load and refreshes after status changes. Only `date` and `event_count`
are used; other fields are surfaced in the date cell if present.

### `GET /api/v1/segments?date=YYYY-MM-DD`

Returns `{"date","segments":[Segment, …]}`. The Portal uses every field listed
in the spec block below for timeline drawing and gap mapping.

### `GET /api/v1/segments/{id}/gaps`

Returns `{"segment_id","capture_rate_hz","time_quality","started_at_utc",
"gaps":[{"offset_frames","lost_frames","kind","estimate_source","at_utc"}, …]}`.
This is the authoritative mapping table. The Portal caches it per segment for
the lifetime of the page so timeline draw, click-to-seek, and the playback
head all use the same data.

### `GET /api/v1/events?date=YYYY-MM-DD`

Returns `{"date","events":[Event, …]}`. The Portal renders the list from this.

### `GET /api/v1/events/{id}`

Returns `{"event":Event,"segment":Segment}`. The Portal calls this on event
click to obtain the authoritative Segment (handles the case where the event
sits on a segment boundary) and to show full event metadata.

### `PATCH /api/v1/events/{id}`

Body: `{"review_status":"snore" | "not_snore" | "unreviewed"}`.
Returns `{"event":Event}`. The Portal calls this on the review buttons and
only updates local state from the returned `event`. Other values are rejected
client-side.

### `GET /api/v1/segments/{id}/audio`

Stream of `audio/wav`. Supports `Range`. The Portal does **not** construct
Range requests itself; it sets `audio.src` and lets the browser do native
HTTP range on play. Known server replies:

- `200` — playable.
- `206` — partial (native seek; browser handles).
- `409` — segment still recording. The Portal treats this as "录音进行中,
  暂不可播放".
- `410` — segment's audio has been deleted by retention. The Portal treats
  this as "录音已过期".
- `404` — segment missing. Shown as "录音不存在".

The Portal never tries to play a segment with `status === "deleted"` or
`status === "missing"` even before the audio endpoint is hit.

## Object shapes used by the Portal

### `Segment`

Fields consumed by the Portal:

| Field | Type | Use |
|---|---|---|
| `id` | string | key for audio + gaps lookup |
| `started_at_utc` | RFC 3339 | wall-clock anchor |
| `ended_at_utc` | RFC 3339 \| null | end label; `null` for `recording` |
| `capture_rate_hz` | integer | gap map divisor and reverse map |
| `channels` | integer | metadata only |
| `sample_format` | string | metadata only |
| `frame_count` | integer | timeline end + post-padding |
| `lost_frames_total` | integer | summary only (true mapping uses gaps) |
| `time_quality` | `"synced" \| "unsynced" \| "corrected"` | timeline style |
| `status` | `"recording" \| "complete" \| "interrupted" \| "deleted" \| "missing"` | segment lifecycle UI |
| `end_reason` | string | metadata only |
| `boot_id` | string | metadata only |
| `clock_drift_ppm` | number \| null | metadata only |

Other fields listed in the spec are accepted if the server sends them but
not used.

### `Event`

Fields consumed:

| Field | Type | Use |
|---|---|---|
| `id` | string | click handler key + PATCH target |
| `segment_id` | string | matches to a segment in the day list |
| `start_offset_frames` | integer | click → `audio.currentTime = max(start/capture_rate_hz − 2, 0)` |
| `end_offset_frames` | integer | end label |
| `started_at_utc` | RFC 3339 | list "开始" column |
| `ended_at_utc` | RFC 3339 | list "结束" column |
| `duration_ms` | integer | list "时长" column |
| `rule_score` | number \| null | list "分数" column (renders `—` when null) |
| `peak_dbfs` | number \| null | metadata popup |
| `mean_level_dbfs` | number \| null | metadata popup |
| `mean_band_ratio` | number \| null | metadata popup |
| `noise_floor_dbfs` | number \| null | metadata popup |
| `end_reason` | string | metadata popup |
| `review_status` | `"unreviewed" \| "snore" \| "not_snore"` | list "标签" + buttons |
| `continued` | integer (0/1) | "续" badge |
| `detector_version` | string | metadata popup |
| `created_at_utc` | RFC 3339 | metadata popup |

### `Gap`

Fields consumed:

| Field | Type | Use |
|---|---|---|
| `offset_frames` | integer | wall mapping key |
| `lost_frames` | integer | wall mapping addend |
| `kind` | string | timeline break label |
| `estimate_source` | string | timeline break subtitle |
| `at_utc` | RFC 3339 | metadata popup |

## Timeline mapping contract

Per §4.3 and §9 the Portal draws time using the gap table, never by
differencing UTC timestamps. Implementation:

```
sort gaps by offset_frames ascending
for any stored-frame offset o >= 0:
    cumLost(o) = Σ lost_frames for gap with offset_frames <= o
    wall(o)    = started_at_utc + (o + cumLost(o)) / capture_rate_hz
```

Playback head (audio.currentTime in seconds):

```
samplePos = audio.currentTime * capture_rate_hz        // browser-reported
// inverse: find the largest offset o such that o + cumLost(o) <= samplePos
// (binary search on the cumulative table)
o = inverseOffset(samplePos)
wallHead = started_at_utc + samplePos / capture_rate_hz    // display = wall(o)
```

Because the audio is continuous PCM (no silence inserted in gaps; gaps are
just *missing* frames), the browser-reported `currentTime` *is* the wall-clock
distance from segment start only if the gap table is consulted. Without the
gap correction, `currentTime × capture_rate_hz` would land in the middle of a
gap and the head would slide behind the visible timeline. The Portal does the
correction.

## States the Portal must show explicitly (§9)

- 加载 — first load, before any request returns.
- 空日期 — no events for the selected date.
- 无录音日期 — date with `segment_count === 0` shows "该日期无录音" if it
  appears in `/days`.
- 无设备 — `microphone.state === "disconnected"` shows in the mic pill; if no
  recording has ever produced audio the timeline shows "尚未产生录音".
- 录音中断 — segment `status === "interrupted"` is rendered with a dashed
  border and a "异常中断，gap 信息可能不完整" caption.
- 时间未校准 — segment `time_quality === "unsynced"` is rendered with a
  dashed fill and a "时间未校准" caption.
- 录音已过期 — segment `status === "deleted"` or the audio endpoint returns
  410: the player row says "录音已过期" with no `<audio>` element.
- 录音不存在 — segment `status === "missing"` or audio endpoint 404: same
  shape as "录音已过期" but with code "missing".
- 录音进行中 — `status === "recording"` and audio endpoint 409: the player
  row says "录音进行中，暂不可播放".
- 磁盘告警 — `storage.pressure !== "ok"` or `storage.recording_stopped_low_disk`
  is true: top-bar shows the alert pill.
- 服务断开 — `/status` poll fails or returns non-JSON: full top-bar goes
  red, status banner stays until a successful poll.
- gap 检测降级 — `capture.gap_detection_limited === true`: the timeline
  carries a small "gap 检测降级" caption; mappings remain correct because
  `lost_frames` is still reported.

## Things the Portal does NOT do

- No autoplay (§9).
- No detection-coverage indicator (§4.3 known limit).
- No UTC-difference math for timeline drawing (§4.3, §9).
- No optimistic review-status update on error.
- No external assets (no CDN, no fonts, no images).