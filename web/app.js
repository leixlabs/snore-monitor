/*
 * Snore Monitor Portal — client.
 *
 * Plain JS, no modules, no bundler. Loaded as `app.js` next to `index.html`
 * and `app.css`. Talks to the Rust HTTP API described in `web/README.md`.
 *
 * All time math goes through the gap table (SPEC §4.3). Wall-clock deltas
 * are never used to position the playhead or draw the timeline.
 *
 * High-level flow:
 *   1. pollStatus() on a 5 s timer and on visibility change
 *   2. loadDays() once on boot, refresh on date selection
 *   3. on date select: loadSegments() + loadEvents() in parallel
 *   4. drawTimeline() uses cached gap tables per segment
 *   5. on event click: loadEventDetail() → loadSegmentGaps() → mountAudio()
 *   6. audio timeupdate → inverseOffset → drawPlayhead() on the same scale
 */
'use strict';

(function () {
  // ----------------------------------------------------------------
  // Constants
  // ----------------------------------------------------------------

  var API_BASE = '/api/v1';
  var STATUS_POLL_MS = 5000;
  var DAYS_WINDOW_DAYS = 60;
  var LOCAL_DATE_FMT_LEN = 10; // "YYYY-MM-DD"
  var PRE_CONTEXT_SECONDS = 2;

  var REVIEW_VALUES = ['snore', 'not_snore', 'unreviewed'];

  // ----------------------------------------------------------------
  // Tiny DOM helpers (kept local to avoid any global requirement)
  // ----------------------------------------------------------------

  function $(sel, root) {
    return (root || document).querySelector(sel);
  }
  function queryAll(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }

  function el(tag, attrs, children) {
    var node = document.createElement(tag);
    if (attrs) {
      for (var k in attrs) {
        if (!Object.prototype.hasOwnProperty.call(attrs, k)) continue;
        var v = attrs[k];
        if (v == null || v === false) continue;
        if (k === 'class') node.className = v;
        else if (k === 'text') node.textContent = v;
        else if (k === 'html') node.innerHTML = v;
        else if (k === 'style' && typeof v === 'object') {
          for (var sk in v) node.style[sk] = v[sk];
        }
        else if (k.indexOf('data-') === 0) node.setAttribute(k, v);
        else if (k.indexOf('on') === 0 && typeof v === 'function') {
          node.addEventListener(k.slice(2).toLowerCase(), v);
        }
        else node.setAttribute(k, v);
      }
    }
    if (children) {
      for (var i = 0; i < children.length; i++) {
        var c = children[i];
        if (c == null || c === false) continue;
        if (typeof c === 'string') node.appendChild(document.createTextNode(c));
        else node.appendChild(c);
      }
    }
    return node;
  }

  function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

  function setText(sel, text) {
    var n = $(sel);
    if (n) n.textContent = text;
  }

  function fmtBytes(n) {
    if (n == null || isNaN(n)) return '—';
    if (n < 1024) return n + ' B';
    var units = ['KiB', 'MiB', 'GiB', 'TiB'];
    var v = n / 1024; var i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return v.toFixed(v >= 100 ? 0 : (v >= 10 ? 1 : 2)) + ' ' + units[i];
  }

  function fmtPercent(n) {
    if (n == null || isNaN(n)) return '—';
    return (Math.round(n * 1000) / 10).toFixed(1) + '%';
  }

  function fmtPctRaw(num, den) {
    if (num == null || den == null || !den) return 0;
    return Math.max(0, Math.min(100, (num / den) * 100));
  }

  function pad2(n) { return n < 10 ? '0' + n : '' + n; }

  function fmtClock(d) {
    return pad2(d.getHours()) + ':' + pad2(d.getMinutes()) + ':' + pad2(d.getSeconds());
  }

  function fmtLocalDate(d) {
    return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate());
  }

  function todayLocalDate() {
    return fmtLocalDate(new Date());
  }

  function shiftDate(dateStr, deltaDays) {
    // dateStr is YYYY-MM-DD; treat as UTC midnight to avoid TZ drift, then
    // re-format using local fields.
    var parts = dateStr.split('-');
    var d = new Date(Date.UTC(+parts[0], +parts[1] - 1, +parts[2]));
    d.setUTCDate(d.getUTCDate() + deltaDays);
    return d.getUTCFullYear() + '-' + pad2(d.getUTCMonth() + 1) + '-' + pad2(d.getUTCDate());
  }

  function parseRfc3339(s) {
    if (!s) return null;
    var d = new Date(s);
    if (isNaN(d.getTime())) return null;
    return d;
  }

  function fmtDurationMs(ms) {
    if (ms == null || isNaN(ms)) return '—';
    var s = Math.round(ms / 1000);
    if (s < 60) return s + ' 秒';
    var m = Math.floor(s / 60);
    var rs = s % 60;
    if (m < 60) return m + ' 分 ' + (rs < 10 ? '0' : '') + rs + ' 秒';
    var h = Math.floor(m / 60);
    var rm = m % 60;
    return h + ' 时 ' + (rm < 10 ? '0' : '') + rm + ' 分';
  }

  function fmtScore(s) {
    if (s == null || isNaN(s)) return '—';
    return (Math.round(s * 1000) / 1000).toFixed(3);
  }

  function clamp(x, lo, hi) { return Math.max(lo, Math.min(hi, x)); }

  // ----------------------------------------------------------------
  // State
  // ----------------------------------------------------------------

  var state = {
    status: null,           // last good status object
    statusError: null,      // string or null (server disconnected)
    days: [],               // [{date, segment_count, event_count, duration_seconds, lost_frames_total}]
    daysError: null,
    currentDate: null,      // 'YYYY-MM-DD'
    segmentsByDate: {},     // date -> Segment[]
    eventsByDate: {},       // date -> Event[]
    gapsBySegment: {},      // segment_id -> {capture_rate_hz, time_quality, started_at_utc, gaps:[...]}
    audioBySegment: {},     // segment_id -> {audio: HTMLAudioElement, gaps: Gap[], rate: int, started: Date}
    currentSegmentId: null, // currently rendered on the player
    currentEventId: null,   // selected event
    detailCache: {},        // event_id -> {event, segment}
    statusPollTimer: null,
    pendingPatch: null,     // {eventId, status, attempts, resolve, reject}
  };

  // ----------------------------------------------------------------
  // Fetch helper
  // ----------------------------------------------------------------

  function apiErrorFromResponse(status, body) {
    if (body && typeof body === 'object' && body.error && body.error.code) {
      return {
        status: status,
        code: body.error.code,
        message: body.error.message || ('HTTP ' + status),
      };
    }
    return { status: status, code: 'http_' + status, message: 'HTTP ' + status };
  }

  function buildUrl(path, params) {
    var qs = '';
    if (params) {
      var parts = [];
      for (var k in params) {
        if (!Object.prototype.hasOwnProperty.call(params, k)) continue;
        var v = params[k];
        if (v == null) continue;
        parts.push(encodeURIComponent(k) + '=' + encodeURIComponent(v));
      }
      if (parts.length) qs = '?' + parts.join('&');
    }
    return API_BASE + path + qs;
  }

  function fetchJSON(path, params) {
    return fetch(buildUrl(path, params), {
      headers: { 'Accept': 'application/json' },
      credentials: 'same-origin',
    }).then(function (resp) {
      var ct = resp.headers.get('content-type') || '';
      if (resp.status === 204) return null;
      if (ct.indexOf('application/json') === -1) {
        return resp.text().then(function (txt) {
          throw apiErrorFromResponse(resp.status, { error: { code: 'bad_content_type', message: 'expected JSON, got: ' + (txt || '').slice(0, 80) } });
        });
      }
      return resp.json().then(function (body) {
        if (!resp.ok) throw apiErrorFromResponse(resp.status, body);
        return body;
      }).catch(function (err) {
        if (err && err.status) throw err;
        throw { status: resp.status, code: 'parse_error', message: 'invalid JSON: ' + (err && err.message || '') };
      });
    });
  }

  function patchJSON(path, body) {
    return fetch(buildUrl(path), {
      method: 'PATCH',
      headers: { 'Accept': 'application/json', 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify(body),
    }).then(function (resp) {
      var ct = resp.headers.get('content-type') || '';
      if (ct.indexOf('application/json') === -1) {
        return resp.text().then(function (txt) {
          throw apiErrorFromResponse(resp.status, { error: { code: 'bad_content_type', message: 'expected JSON, got: ' + (txt || '').slice(0, 80) } });
        });
      }
      return resp.json().then(function (j) {
        if (!resp.ok) throw apiErrorFromResponse(resp.status, j);
        return j;
      }).catch(function (err) {
        if (err && err.status) throw err;
        throw { status: resp.status, code: 'parse_error', message: 'invalid JSON: ' + (err && err.message || '') };
      });
    });
  }

  // ----------------------------------------------------------------
  // Status bar
  // ----------------------------------------------------------------

  function renderStatus() {
    var banner = $('#banner');
    var pills = queryAll('#status-pills .pill');
    var pillByKey = {};
    pills.forEach(function (p) { pillByKey[p.getAttribute('data-key')] = p; });

    if (state.statusError) {
      banner.className = 'banner banner-banner-bad';
      banner.textContent = '服务断开：' + state.statusError;
      // Mark all pills as unknown / bad
      ['service', 'microphone', 'recording', 'detector', 'capture'].forEach(function (k) {
        var p = pillByKey[k];
        if (!p) return;
        var v = p.querySelector('[data-bind="value"]');
        if (v) v.textContent = '未知';
        p.className = 'pill pill-state-bad';
      });
      renderDisk(null);
      return;
    }

    var s = state.status;
    if (!s) {
      banner.className = 'banner banner-hidden';
      banner.textContent = '';
      return;
    }

    // Banner: server-side alerts
    var alerts = [];
    if (s.storage && s.storage.recording_stopped_low_disk) {
      alerts.push('磁盘空间不足，录音已停止。');
    }
    if (s.capture && s.capture.gap_detection_limited) {
      alerts.push('gap 检测降级（仅依赖计数器/错误回调）。');
    }
    if (s.recording && s.recording.state === 'interrupted') {
      alerts.push('录音异常中断。');
    }
    if (s.service && s.service.state === 'degraded') {
      alerts.push('服务处于降级状态。');
    }
    if (alerts.length) {
      banner.className = 'banner banner-banner-warn';
      banner.textContent = alerts.join(' ');
    } else {
      banner.className = 'banner banner-hidden';
      banner.textContent = '';
    }

    setPill(pillByKey.service, s.service && s.service.state === 'ok' ? 'ok' :
                            (s.service && s.service.state === 'degraded' ? 'degraded' : 'unknown'),
                            s.service && s.service.state ? s.service.state : '未知');

    var mic = s.microphone || {};
    var micState = mic.state || 'unknown';
    setPill(pillByKey.microphone,
      micState === 'connected' ? 'ok' :
      micState === 'disconnected' ? 'bad' : 'neutral',
      mic.device ? (micState + ' · ' + truncate(mic.device, 18)) : micState);

    var rec = s.recording || {};
    setPill(pillByKey.recording,
      rec.state === 'recording' ? 'ok' :
      rec.state === 'idle' ? 'neutral' :
      rec.state === 'interrupted' ? 'warn' : 'neutral',
      rec.state || '未知');

    var det = s.detector || {};
    var detEvents = (det.events_total != null) ? (' · ' + det.events_total + ' 事件') : '';
    setPill(pillByKey.detector,
      det.state === 'ok' ? 'ok' :
      det.state === 'degraded' ? 'warn' : 'neutral',
      (det.state || '未知') + detEvents);

    var cap = s.capture || {};
    setPill(pillByKey.capture,
      cap.gap_detection_limited ? 'warn' :
      (cap.lost_frames_total > 0 ? 'warn' : 'ok'),
      '丢帧 ' + (cap.lost_frames_total != null ? cap.lost_frames_total : 0));

    renderDisk(s.storage);
  }

  function setPill(pill, stateName, valueText) {
    if (!pill) return;
    var v = pill.querySelector('[data-bind="value"]');
    if (v) v.textContent = valueText;
    var klass = 'pill';
    if (stateName === 'ok') klass += ' pill-state-ok';
    else if (stateName === 'warn' || stateName === 'degraded') klass += ' pill-state-warn';
    else if (stateName === 'bad') klass += ' pill-state-bad';
    else klass += ' pill-state-neutral';
    pill.className = klass;
  }

  function renderDisk(storage) {
    var fill = $('[data-bind="disk-fill"]');
    var text = $('[data-bind="disk-text"]');
    var meta = $('[data-bind="disk-meta"]');
    var pill = $('.pill[data-key="disk"]');
    var pillValue = pill && pill.querySelector('[data-bind="value"]');

    if (!storage || storage.max_bytes == null) {
      if (fill) fill.style.width = '0%';
      if (text) text.textContent = '—';
      if (meta) meta.textContent = '';
      if (pillValue) pillValue.textContent = '未知';
      if (pill) pill.className = 'pill pill-state-neutral';
      return;
    }

    var pct = fmtPctRaw(storage.used_bytes, storage.max_bytes);
    if (fill) {
      fill.style.width = pct.toFixed(2) + '%';
      fill.classList.remove('disk-bar-fill-state-warn', 'disk-bar-fill-state-bad');
      if (pct >= 90) fill.classList.add('disk-bar-fill-state-bad');
      else if (pct >= 75) fill.classList.add('disk-bar-fill-state-warn');
    }

    if (text) {
      text.textContent = fmtBytes(storage.used_bytes) + ' / ' + fmtBytes(storage.max_bytes) + ' (' + fmtPercent(pct / 100) + ')';
    }

    var pressure = storage.pressure || 'ok';
    var pillStateName = pressure === 'ok' ? 'ok' : (pressure === 'critical' ? 'bad' : 'warn');
    setPill(pill, pillStateName, pressure);

    var parts = [];
    if (storage.free_bytes != null) parts.push('剩余 ' + fmtBytes(storage.free_bytes));
    if (storage.cleanup_target_bytes != null) parts.push('目标 ' + fmtBytes(storage.cleanup_target_bytes));
    if (storage.reserve_bytes != null) parts.push('保留 ' + fmtBytes(storage.reserve_bytes));
    if (storage.deleted_segments != null) parts.push('已删段 ' + storage.deleted_segments);
    if (storage.missing_segments != null) parts.push('缺失 ' + storage.missing_segments);
    if (storage.recording_stopped_low_disk) parts.push('录音已停止');
    if (meta) meta.textContent = parts.join(' · ');
  }

  function truncate(s, n) {
    if (!s) return '';
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  }

  // ----------------------------------------------------------------
  // Status polling
  // ----------------------------------------------------------------

  function pollStatus() {
    return fetchJSON('/status').then(function (body) {
      state.status = body;
      state.statusError = null;
    }).catch(function (err) {
      state.statusError = err && err.message ? err.message : ('HTTP ' + (err && err.status || '???'));
    }).then(function () { renderStatus(); });
  }

  function startStatusPolling() {
    stopStatusPolling();
    state.statusPollTimer = setInterval(pollStatus, STATUS_POLL_MS);
  }

  function stopStatusPolling() {
    if (state.statusPollTimer) {
      clearInterval(state.statusPollTimer);
      state.statusPollTimer = null;
    }
  }

  // ----------------------------------------------------------------
  // Days & date selection
  // ----------------------------------------------------------------

  function loadDays() {
    var list = $('#date-list');
    list.innerHTML = '';
    list.appendChild(el('div', { class: 'state state-loading', text: '加载中…' }));

    var today = todayLocalDate();
    var from = shiftDate(today, -(DAYS_WINDOW_DAYS - 1));
    return fetchJSON('/days', { from: from, to: today }).then(function (body) {
      state.days = (body && body.days) || [];
      state.daysError = null;
      renderDateList();
      // Auto-select today if it has data, else the most recent day
      var pick = null;
      for (var i = 0; i < state.days.length; i++) {
        if (state.days[i].date === today) { pick = today; break; }
      }
      if (!pick && state.days.length) pick = state.days[0].date;
      if (pick) selectDate(pick);
      else clearDay();
    }).catch(function (err) {
      state.days = [];
      state.daysError = err.message || '加载失败';
      renderDateList();
      clearDay();
    });
  }

  function renderDateList() {
    var list = $('#date-list');
    clear(list);

    if (state.daysError) {
      list.appendChild(el('div', { class: 'state state-error', text: '加载失败：' + state.daysError }));
      return;
    }
    if (!state.days.length) {
      list.appendChild(el('div', { class: 'state state-empty', text: '过去 60 天内尚无录音。' }));
      return;
    }

    state.days.forEach(function (d) {
      var btn = el('button', {
        type: 'button',
        class: 'date-btn' + (d.date === state.currentDate ? ' date-btn-active' : ''),
        'data-date': d.date,
        onclick: function () { selectDate(d.date); },
      }, [
        el('span', { class: 'date-btn-main', text: formatHumanDate(d.date) }),
        el('span', { class: 'date-btn-meta', text:
          (d.event_count != null ? d.event_count + ' 事件' : '') +
          (d.segment_count != null ? ' · ' + d.segment_count + ' 段' : '')
        }),
      ]);
      list.appendChild(btn);
    });
  }

  function formatHumanDate(dateStr) {
    var parts = dateStr.split('-');
    var d = new Date(+parts[0], +parts[1] - 1, +parts[2]);
    var today = new Date();
    today.setHours(0, 0, 0, 0);
    var diffDays = Math.round((today - d) / (24 * 3600 * 1000));
    var label = d.getFullYear() + '年' + pad2(d.getMonth() + 1) + '月' + pad2(d.getDate()) + '日';
    if (diffDays === 0) label += '（今天）';
    else if (diffDays === 1) label += '（昨天）';
    return label;
  }

  function selectDate(dateStr) {
    if (state.currentDate === dateStr) return;
    state.currentDate = dateStr;
    // Highlight
    queryAll('.date-btn').forEach(function (b) {
      b.classList.toggle('date-btn-active', b.getAttribute('data-date') === dateStr);
    });
    loadDay();
  }

  function clearDay() {
    state.currentDate = null;
    setText('#day-title', '请选择左侧日期');
    setText('#day-summary', '');
    var tl = $('#timeline');
    clear(tl);
    tl.appendChild(el('div', { class: 'timeline-empty', text: '请选择有数据的日期。' }));
    var ev = $('#events');
    clear(ev);
    ev.appendChild(el('div', { class: 'state state-empty', text: '无事件。' }));
    setText('#events-count', '');
    resetPlayer();
  }

  // ----------------------------------------------------------------
  // Day loading (segments + events)
  // ----------------------------------------------------------------

  function loadDay() {
    if (!state.currentDate) return clearDay();
    var date = state.currentDate;

    setText('#day-title', formatHumanDate(date));
    var tl = $('#timeline');
    clear(tl);
    tl.appendChild(el('div', { class: 'timeline-empty', text: '加载中…' }));
    var ev = $('#events');
    clear(ev);
    ev.appendChild(el('div', { class: 'state state-loading', text: '事件加载中…' }));
    resetPlayer();
    setText('#events-count', '');

    var segP = state.segmentsByDate[date]
      ? Promise.resolve(state.segmentsByDate[date])
      : fetchJSON('/segments', { date: date }).then(function (b) {
          var segs = (b && b.segments) || [];
          state.segmentsByDate[date] = segs;
          return segs;
        });

    var evtP = state.eventsByDate[date]
      ? Promise.resolve(state.eventsByDate[date])
      : fetchJSON('/events', { date: date }).then(function (b) {
          var evs = (b && b.events) || [];
          state.eventsByDate[date] = evs;
          return evs;
        });

    Promise.all([segP, evtP]).then(function (results) {
      var segs = results[0];
      var evs = results[1];
      renderTimeline(segs);
      renderEventList(evs);
      var dayInfo = state.days.find(function (d) { return d.date === date; });
      var summaryParts = [];
      if (dayInfo) {
        if (dayInfo.segment_count != null) summaryParts.push(dayInfo.segment_count + ' 段录音');
        if (dayInfo.duration_seconds != null) summaryParts.push('总时长 ' + fmtDurationMs(dayInfo.duration_seconds * 1000));
        if (dayInfo.lost_frames_total) summaryParts.push('丢帧 ' + dayInfo.lost_frames_total);
      }
      summaryParts.push(evs.length + ' 个候选事件');
      setText('#day-summary', summaryParts.join(' · '));
    }).catch(function (err) {
      tl.innerHTML = '';
      tl.appendChild(el('div', { class: 'timeline-empty', text: '加载失败：' + (err && err.message || '') }));
      ev.innerHTML = '';
      ev.appendChild(el('div', { class: 'state state-error', text: '加载失败：' + (err && err.message || '') }));
    });
  }

  // ----------------------------------------------------------------
  // Gap table helpers — SPEC §4.3
  // ----------------------------------------------------------------

  function ensureGaps(segmentId) {
    if (state.gapsBySegment[segmentId]) return Promise.resolve(state.gapsBySegment[segmentId]);
    return fetchJSON('/segments/' + encodeURIComponent(segmentId) + '/gaps').then(function (body) {
      var gaps = (body && body.gaps) || [];
      var info = {
        capture_rate_hz: body && body.capture_rate_hz,
        time_quality: body && body.time_quality,
        started_at_utc: body && body.started_at_utc,
        gaps: gaps.slice().sort(function (a, b) { return a.offset_frames - b.offset_frames; }),
      };
      state.gapsBySegment[segmentId] = info;
      return info;
    });
  }

  // Compute cumulative lost-frames at offset o (inclusive).
  function cumLostAt(info, o) {
    var gaps = info.gaps;
    if (!gaps.length) return 0;
    // gaps sorted by offset_frames; binary search for last gap with offset_frames <= o
    var lo = 0, hi = gaps.length - 1, best = -1;
    while (lo <= hi) {
      var mid = (lo + hi) >>> 1;
      if (gaps[mid].offset_frames <= o) { best = mid; lo = mid + 1; }
      else hi = mid - 1;
    }
    if (best < 0) return 0;
    var sum = 0;
    for (var i = 0; i <= best; i++) sum += gaps[i].lost_frames;
    return sum;
  }

  function wallAt(info, o) {
    var started = parseRfc3339(info.started_at_utc);
    if (!started) return null;
    var cum = cumLostAt(info, o);
    return new Date(started.getTime() + ((o + cum) * 1000) / info.capture_rate_hz);
  }

  // Reverse: given a sample position (audio.currentTime * capture_rate_hz),
  // find o such that s(o) = o + cumLost(o) is closest to samplePos from below.
  // SPEC §4.3: wall(o) = started + (o + Σ lost[gap.offset <= o]) / rate.
  // Forward map s(o) = o + cumLost(o). Stored region [prevOff, gapOff)
  // contributes samplePos = o + cum (cum does not yet include gapOff's lost).
  // Browsers also cap `audio.currentTime` to the file's stored-frame count, but
  // we still clamp at the audio end so a stale cached gap table cannot push
  // the head past the file.
  function inverseOffset(info, samplePos) {
    if (samplePos <= 0) return 0;
    var gaps = info.gaps;
    var o;
    if (!gaps.length) {
      o = samplePos;
    } else {
      var cum = 0;
      var prevOff = 0;
      var found = false;
      for (var i = 0; i < gaps.length; i++) {
        var gapOff = gaps[i].offset_frames;
        var gapLost = gaps[i].lost_frames;
        // Stored region [prevOff, gapOff): samplePos in [prevOff + cum, gapOff + cum)
        if (samplePos < gapOff + cum) {
          o = samplePos - cum;
          if (o < prevOff) o = prevOff;
          if (o > gapOff) o = gapOff;
          found = true;
          break;
        }
        // Gap itself: samplePos in [gapOff + cum, gapOff + cum + gapLost).
        if (samplePos < gapOff + cum + gapLost) {
          o = gapOff; found = true; break;
        }
        cum += gapLost;
        prevOff = gapOff;
      }
      if (!found) {
        // Past last gap: stored region [prevOff, ∞), cumLost = cum.
        o = samplePos - cum;
        if (o < prevOff) o = prevOff;
      }
    }
    // Clamp to a sane stored-frame end if we have it cached.
    var cached = state.audioBySegment[segmentIdFor(info)];
    if (cached && cached.audio && cached.audio.duration && info.capture_rate_hz) {
      var audioEndFrames = cached.audio.duration * info.capture_rate_hz;
      if (o > audioEndFrames) o = audioEndFrames;
    }
    return Math.floor(o);
  }

  // Map an info object back to its segment id via the gapsBySegment registry.
  function segmentIdFor(info) {
    for (var k in state.gapsBySegment) {
      if (state.gapsBySegment[k] === info) return k;
    }
    return null;
  }

  function frameRate(info) {
    return info.capture_rate_hz || 16000;
  }

  // ----------------------------------------------------------------
  // Timeline rendering
  // ----------------------------------------------------------------

  function renderTimeline(segments) {
    var tl = $('#timeline');
    clear(tl);
    var hint = $('#timeline-hint');
    hint.textContent = '';

    if (!segments || !segments.length) {
      tl.appendChild(el('div', { class: 'timeline-empty', text: '该日期没有录音段。' }));
      $('#timeline-axis').innerHTML = '';
      return;
    }

    // Compute day bounds from the segments. The server already filtered by the
    // requested local date; we just span the actual coverage with a small pad.
    var segments = segments.slice().sort(function (a, b) {
      return parseRfc3339(a.started_at_utc) - parseRfc3339(b.started_at_utc);
    });

    // Day range: use the span from earliest to latest segment, padded by
    // half an hour on each side. The server filters segments and events
    // by the requested local date, but the spans cross local-midnight
    // boundaries when there is a tz offset, so we can't just rely on
    // local-midnight here. (Day navigation still keys on the server's
    // local date; the portal only uses this for drawing axes.)
    var firstSeg = segments[0];
    var lastSeg = segments[segments.length - 1];
    var firstStart = parseRcf3339Min(firstSeg);
    var lastEnd = parseRcf3339Max(segments);
    var dayStartMs = firstStart.getTime() - 30 * 60 * 1000;
    var dayEndMs = lastEnd.getTime() + 30 * 60 * 1000;
    var totalMs = Math.max(dayEndMs - dayStartMs, 60 * 1000);

    function pct(t) { return clamp((t - dayStartMs) / totalMs, 0, 1) * 100; }

    // Build a synthetic "current segment" playhead element up front.
    var tlHead = el('div', { class: 'tl-head-line', 'data-role': 'head' });
    var tlHeadMarker = el('div', { class: 'tl-head-marker', 'data-role': 'head-marker', style: { display: 'none' } });

    var tlRowCoverage = el('div', { class: 'tl-row tl-row-coverage', 'data-role': 'coverage' });
    var tlRowEvents = el('div', { class: 'tl-row tl-row-events', 'data-role': 'events' });

    // Draw segments
    var anyUnsynced = false;
    var anyInterrupted = false;
    segments.forEach(function (seg) {
      var start = parseRfc3339(seg.started_at_utc);
      if (!start) return;
      // segment end: use wall(frame_count) for ended segments; for recording
      // use the current wall time.
      var segEndWall;
      var endedAt = parseRfc3339(seg.ended_at_utc);
      if (seg.status === 'recording') {
        segEndWall = new Date();
      } else if (endedAt) {
        segEndWall = endedAt;
      } else {
        segEndWall = start; // unknown
      }

      var covClass = 'tl-coverage';
      covClass += ' tl-coverage-status-' + (seg.status || 'complete');
      if (seg.time_quality === 'unsynced') { covClass += ' tl-coverage-quality-unsynced'; anyUnsynced = true; }
      if (seg.time_quality === 'corrected') covClass += ' tl-coverage-quality-corrected';

      var leftPct = pct(start.getTime());
      var rightPct = pct(segEndWall.getTime());
      if (rightPct < leftPct + 0.1) rightPct = leftPct + 0.1;

      var cov = el('div', {
        class: covClass,
        style: { left: leftPct + '%', width: (rightPct - leftPct) + '%' },
        'data-segment-id': seg.id,
        title: (seg.id || '') + '\n' +
               (seg.started_at_utc || '') + ' → ' + (seg.ended_at_utc || '(recording)') + '\n'
               + 'time_quality=' + (seg.time_quality || '?') + ', status=' + (seg.status || '?'),
      });
      // Tiny per-segment label
      var meta = el('div', {
        class: 'segment-line-meta',
        style: { left: leftPct + '%' },
        text: (seg.time_quality === 'unsynced' ? '时间未校准 · ' : '') +
              (seg.status === 'interrupted' ? '异常中断 · ' : '') +
              (seg.status === 'recording' ? '录音中 · ' : '') +
              fmtClock(start),
      });
      cov.appendChild(meta);
      tlRowCoverage.appendChild(cov);

      if (seg.status === 'interrupted') anyInterrupted = true;
    });

    // Hint text
    var hintParts = [];
    if (anyUnsynced) hintParts.push('虚线：时间未校准');
    if (anyInterrupted) hintParts.push('异常中断段 gap 信息可能不完整');
    if (segments.some(function (s) { return s.status === 'recording'; })) {
      hintParts.push('含正在录音的段');
    }
    hint.textContent = hintParts.join(' · ');

    tl.appendChild(tlRowCoverage);
    tl.appendChild(tlRowEvents);
    tl.appendChild(tlHead);
    tlHead.appendChild(tlHeadMarker);

    // Draw axis with hour labels at local-midnight-aligned times within the span.
    drawTimelineAxis(dayStartMs, dayEndMs);

    // Events: draw later when we have segments, to share the gap table for click handlers.
    // We can't draw time yet because we need gaps. Mark a flag.
    tl.setAttribute('data-has-segments', '1');
    tl.__segments = segments;
    tl.__dayStartMs = dayStartMs;
    tl.__dayEndMs = dayEndMs;
    tl.__pct = pct;

    // We render events here too because we already have event data. We need
    // each event's segment gaps to draw the wall position. Load them.
    var date = state.currentDate;
    var events = (state.eventsByDate[date] || []).slice().sort(function (a, b) {
      return parseRfc3339(a.started_at_utc) - parseRfc3339(b.started_at_utc);
    });
    drawTimelineEvents(tl, events, segments, pct);
  }

  function parseRcf3339Max(segments) {
    var max = null;
    segments.forEach(function (s) {
      var t = parseRfc3339(s.ended_at_utc || s.started_at_utc);
      if (t && (!max || t > max)) max = t;
    });
    return max || new Date();
  }

  function parseRcf3339Min(seg) {
    return parseRfc3339(seg.started_at_utc) || parseRfc3339(seg.ended_at_utc) || new Date();
  }

  function drawTimelineAxis(dayStartMs, dayEndMs) {
    var axis = $('#timeline-axis');
    clear(axis);
    // Anchor ticks at the nearest local-midnight-aligned hour, step every 3 hours.
    var startD = new Date(dayStartMs);
    startD.setMinutes(0, 0, 0);
    // Step back so the first tick <= dayStartMs
    while (startD.getTime() > dayStartMs) startD.setHours(startD.getHours() - 1);
    while (startD.getHours() % 3 !== 0) startD.setHours(startD.getHours() - 1);
    var stepMs = 3 * 3600 * 1000;
    for (var t = startD.getTime(); t <= dayEndMs; t += stepMs) {
      if (t < dayStartMs) continue;
      var d = new Date(t);
      var p = ((t - dayStartMs) / (dayEndMs - dayStartMs)) * 100;
      axis.appendChild(el('div', {
        class: 'timeline-axis-tick',
        style: { left: p + '%' },
        text: pad2(d.getHours()) + ':00',
      }));
    }
  }

  function drawTimelineEvents(tl, events, segments, pct) {
    var row = tl.querySelector('[data-role="events"]');
    if (!row) return;
    clear(row);

    if (!events.length) {
      // No events for this day; leave the row empty (axis still visible).
      return;
    }

    // Pre-fetch gap tables we don't have yet so we can draw positions.
    var segsNeeded = {};
    events.forEach(function (e) { segsNeeded[e.segment_id] = true; });
    var promises = Object.keys(segsNeeded).map(function (id) {
      return ensureGaps(id).catch(function () { return null; });
    });
    Promise.all(promises).then(function () {
      events.forEach(function (ev) {
        var info = state.gapsBySegment[ev.segment_id];
        if (!info) return;
        var wallStart = wallAt(info, ev.start_offset_frames);
        var wallEnd = wallAt(info, ev.end_offset_frames);
        if (!wallStart || !wallEnd) return;
        var leftPct = pct(wallStart.getTime());
        var rightPct = pct(wallEnd.getTime());
        if (rightPct < leftPct + 0.05) rightPct = leftPct + 0.1;

        var revClass = 'tl-event';
        if (ev.review_status === 'snore') revClass += ' tl-event-status-snore';
        else if (ev.review_status === 'not_snore') revClass += ' tl-event-status-not_snore';
        if (ev.id === state.currentEventId) revClass += ' tl-event-active';

        var block = el('button', {
          type: 'button',
          class: revClass,
          style: { left: leftPct + '%', width: Math.max(rightPct - leftPct, 0.3) + '%' },
          'data-event-id': ev.id,
          title: fmtClock(wallStart) + ' → ' + fmtClock(wallEnd) + ' · ' +
                 fmtDurationMs(ev.duration_ms) +
                 (ev.rule_score != null ? ' · score ' + fmtScore(ev.rule_score) : ''),
          onclick: function () { selectEvent(ev.id, { fromTimeline: true }); },
        }, [
          ev.continued ? el('span', { class: 'tl-event-label', text: '续 · ' + fmtClock(wallStart) })
                       : el('span', { class: 'tl-event-label', text: fmtClock(wallStart) }),
        ]);
        row.appendChild(block);
      });
    });
  }

  // ----------------------------------------------------------------
  // Events list
  // ----------------------------------------------------------------

  function renderEventList(events) {
    var ev = $('#events');
    clear(ev);
    if (!events || !events.length) {
      ev.appendChild(el('div', { class: 'state state-empty', text: '该日期无候选事件。' }));
      setText('#events-count', '0');
      return;
    }
    setText('#events-count', events.length + ' 个');

    var date = state.currentDate;
    var sorted = events.slice().sort(function (a, b) {
      return parseRfc3339(a.started_at_utc) - parseRfc3339(b.started_at_utc);
    });

    sorted.forEach(function (e) {
      var startedAt = parseRfc3339(e.started_at_utc);
      var endedAt = parseRfc3339(e.ended_at_utc);
      var row = el('button', {
        type: 'button',
        class: 'event-row event-row-status-' + (e.review_status || 'unreviewed'),
        'data-event-id': e.id,
        onclick: function () { selectEvent(e.id, { fromList: true }); },
      }, [
        el('span', { class: 'event-row-time event-col-start', text: startedAt ? fmtClock(startedAt) : '—' }),
        el('span', { class: 'event-row-time event-col-end', text: endedAt ? fmtClock(endedAt) : '—' }),
        el('span', { class: 'event-row-dur', text: fmtDurationMs(e.duration_ms) }),
        el('span', { class: 'event-row-score event-col-score', text: fmtScore(e.rule_score) }),
        el('span', { class: 'event-row-label event-row-label-status-' + (e.review_status || 'unreviewed'), text: labelText(e.review_status) }),
        el('span', { class: 'event-row-continued', text: e.continued ? '续' : '' }),
      ]);
      ev.appendChild(row);
    });
  }

  function labelText(s) {
    if (s === 'snore') return '鼾声';
    if (s === 'not_snore') return '误报';
    return '待审';
  }

  // ----------------------------------------------------------------
  // Player
  // ----------------------------------------------------------------

  function resetPlayer() {
    state.currentEventId = null;
    state.currentSegmentId = null;
    setText('#player-event', '未选择事件');
    setText('#player-segment', '');
    setText('#player-status', '');
    var wrap = $('#audio-wrap');
    clear(wrap);
    ['review-snore', 'review-not', 'review-clear'].forEach(function (id) {
      var b = $('#' + id);
      if (b) b.disabled = true;
    });
  }

  function selectEvent(eventId, opts) {
    opts = opts || {};
    state.currentEventId = eventId;
    // Highlight rows
    queryAll('.event-row').forEach(function (b) {
      b.classList.toggle('event-row-active', b.getAttribute('data-event-id') === eventId);
    });
    queryAll('.tl-event').forEach(function (b) {
      b.classList.toggle('tl-event-active', b.getAttribute('data-event-id') === eventId);
    });

    loadEventDetail(eventId).then(function (detail) {
      var ev = detail.event;
      var seg = detail.segment;

      renderPlayerHeader(ev, seg);

      // Review buttons
      var buttons = ['review-snore', 'review-not', 'review-clear'];
      buttons.forEach(function (id) {
        var b = $('#' + id);
        if (b) b.disabled = false;
      });

      // Mount audio depending on segment status. The authoritative segment
      // comes from /events/{id}; using it (rather than the day list's
      // segment) is what lets us pick the right hour-file at a boundary.
      mountAudio(seg, ev);
    }).catch(function (err) {
      var wrap = $('#audio-wrap');
      clear(wrap);
      wrap.appendChild(el('div', {
        class: 'audio-wrap-state-bad',
        text: '加载事件失败：' + (err && err.message || ''),
      }));
    });
  }

  function loadEventDetail(eventId) {
    if (state.detailCache[eventId]) return Promise.resolve(state.detailCache[eventId]);
    return fetchJSON('/events/' + encodeURIComponent(eventId)).then(function (body) {
      if (!body || !body.event || !body.segment) {
        throw { status: 500, code: 'bad_payload', message: 'events/{id} 返回结构不完整' };
      }
      state.detailCache[eventId] = body;
      return body;
    });
  }

  function renderPlayerHeader(ev, seg) {
    var startedAt = parseRfc3339(ev.started_at_utc);
    setText('#player-event',
      (startedAt ? fmtClock(startedAt) + ' · ' : '') +
      fmtDurationMs(ev.duration_ms) +
      (ev.rule_score != null ? ' · 分数 ' + fmtScore(ev.rule_score) : '') +
      ' · ' + labelText(ev.review_status));
    setText('#player-segment',
      '段 ' + (seg.id || '').slice(0, 8) + ' · ' +
      fmtBytes(seg.size_bytes) + ' · ' +
      (seg.capture_rate_hz || '?') + ' Hz · ' +
      (seg.channels || '?') + ' 通道 · ' +
      'time_quality=' + (seg.time_quality || '?') + ' · ' +
      'status=' + (seg.status || '?'));

    var statusBits = [];
    if (seg.status === 'recording') statusBits.push('录音进行中');
    if (seg.status === 'interrupted') statusBits.push('异常中断（gap 信息可能不完整）');
    if (seg.time_quality === 'unsynced') statusBits.push('时间未校准');
    if (seg.status === 'deleted') statusBits.push('录音已过期');
    if (seg.status === 'missing') statusBits.push('录音不存在');
    setText('#player-status', statusBits.join(' · '));
    var statusEl = $('#player-status');
    statusEl.classList.remove('player-status-state-bad', 'player-status-state-warn');
    if (seg.status === 'deleted' || seg.status === 'missing') {
      statusEl.classList.add('player-status-state-bad');
    } else if (seg.status === 'interrupted' || seg.time_quality === 'unsynced') {
      statusEl.classList.add('player-status-state-warn');
    }
  }

  function refreshPlayerHeaderFromCache() {
    var cached = state.detailCache[state.currentEventId];
    if (!cached) return;
    renderPlayerHeader(cached.event, cached.segment);
  }

  function mountAudio(segment, event) {
    var wrap = $('#audio-wrap');
    clear(wrap);

    // Non-playable states: no audio element, just an explicit message.
    if (segment.status === 'deleted') {
      wrap.appendChild(el('div', { class: 'audio-wrap-state-bad', text: '录音已过期，无法播放。' }));
      return;
    }
    if (segment.status === 'missing') {
      wrap.appendChild(el('div', { class: 'audio-wrap-state-bad', text: '录音不存在，无法播放。' }));
      return;
    }

    // Build <audio controls>; do not autoplay (§9).
    var audio = document.createElement('audio');
    audio.controls = true;
    audio.preload = 'metadata';
    audio.setAttribute('aria-label', '录音播放器');
    audio.src = API_BASE + '/segments/' + encodeURIComponent(segment.id) + '/audio';

    var statusEl = el('div', { class: 'player-status', text: '加载中…' });
    wrap.appendChild(audio);
    wrap.appendChild(statusEl);

    var fatalError = function (msg) {
      statusEl.className = 'player-status player-status-state-bad';
      statusEl.textContent = msg;
      audio.parentNode && audio.parentNode.removeChild(audio);
    };

    audio.addEventListener('error', function () {
      // Map HTTP errors via XHR? The audio element doesn't expose status.
      // Use a HEAD probe for an explicit signal.
      fetch(API_BASE + '/segments/' + encodeURIComponent(segment.id) + '/audio', { method: 'HEAD', credentials: 'same-origin' })
        .then(function (resp) {
          if (resp.status === 410) fatalError('录音已过期，无法播放。');
          else if (resp.status === 409) fatalError('录音进行中，暂不可播放。');
          else if (resp.status === 404) fatalError('录音不存在。');
          else fatalError('音频加载失败（HTTP ' + resp.status + '）。');
        })
        .catch(function () { fatalError('音频加载失败。'); });
    });

    audio.addEventListener('loadedmetadata', function () {
      ensureGaps(segment.id).then(function (info) {
        state.audioBySegment[segment.id] = {
          audio: audio,
          gaps: info.gaps,
          rate: info.capture_rate_hz,
          started: parseRfc3339(info.started_at_utc),
        };
        // Seek to context window
        var rate = info.capture_rate_hz || 16000;
        var sec = Math.max((event.start_offset_frames / rate) - PRE_CONTEXT_SECONDS, 0);
        try { audio.currentTime = sec; } catch (_) {}
        statusEl.className = 'player-status';
        statusEl.textContent = '起始位置 ' + sec.toFixed(2) + ' 秒（事件前 ' + PRE_CONTEXT_SECONDS + ' 秒）';
        // Draw playhead and arm tick handler
        drawPlayheadFromAudio();
        audio.addEventListener('timeupdate', drawPlayheadFromAudio);
      }).catch(function (err) {
        statusEl.className = 'player-status player-status-state-bad';
        statusEl.textContent = '加载 gap 数据失败：' + (err && err.message || '');
      });
    });

    audio.addEventListener('play', drawPlayheadFromAudio);
    audio.addEventListener('seeked', drawPlayheadFromAudio);

    function drawPlayheadFromAudio() {
      if (audio.paused && audio.readyState < 2) return;
      var rec = state.audioBySegment[segment.id];
      if (!rec || !rec.started) return;
      var info = state.gapsBySegment[segment.id];
      if (!info) return;
      var samplePos = audio.currentTime * rec.rate;
      var o = inverseOffset(info, samplePos);
      var wall = wallAt(info, o);
      var tl = $('#timeline');
      var pct = tl.__pct;
      if (!pct) return;
      var leftPct = pct(wall.getTime());
      var marker = tl.querySelector('[data-role="head-marker"]');
      if (!marker) return;
      marker.style.display = '';
      marker.style.left = leftPct + '%';
    }
  }

  // ----------------------------------------------------------------
  // Review (PATCH)
  // ----------------------------------------------------------------

  function setReview(status) {
    var eventId = state.currentEventId;
    if (!eventId) return;
    if (REVIEW_VALUES.indexOf(status) < 0) return;

    // Capture a snapshot of the previous display state so we can revert on failure.
    var cached = state.detailCache[eventId];
    var prevReview = cached && cached.event && cached.event.review_status;
    // Apply optimistically to the list so the user sees feedback, but only
    // mark the cache as "pending" so we can roll back.
    var prevCacheEvent = cached && cached.event ? Object.assign({}, cached.event) : null;
    if (cached) cached.event.review_status = status;

    // Reflect immediately in local event list so the row label updates now.
    patchLocalReview(eventId, status);
    // And in the timeline event block.
    patchTimelineBlockReview(eventId, status);

    patchEvent(eventId, status).then(function (resp) {
      if (cached) cached.event = resp.event;
      // Cache the latest event from the server response.
      var date = state.currentDate;
      var list = state.eventsByDate[date] || [];
      for (var i = 0; i < list.length; i++) {
        if (list[i].id === eventId) { list[i] = resp.event; break; }
      }
      // If the just-patched event is the one currently mounted on the
      // player, refresh the header so the new label is shown.
      if (state.currentEventId === eventId) refreshPlayerHeaderFromCache();
      showNotification(
        status === 'snore' ? '已标记为鼾声' :
        status === 'not_snore' ? '已标记为误报' : '已清除标签');
    }).catch(function (err) {
      // Rollback
      if (cached && prevCacheEvent) cached.event = prevCacheEvent;
      patchLocalReview(eventId, prevReview);
      patchTimelineBlockReview(eventId, prevReview);
      // Retry prompt
      state.pendingPatch = {
        eventId: eventId,
        status: status,
        prevReview: prevReview,
        attempts: 1,
      };
      openRetryModal(err && err.message ? err.message : '服务器未接受本次更新');
    });
  }

  function patchLocalReview(eventId, status) {
    var row = document.querySelector('.event-row[data-event-id="' + cssEscape(eventId) + '"]');
    if (!row) return;
    row.classList.remove('event-row-status-snore', 'event-row-status-not_snore', 'event-row-status-unreviewed');
    row.classList.add('event-row-status-' + (status || 'unreviewed'));
    var lbl = row.querySelector('.event-row-label');
    if (lbl) {
      lbl.className = 'event-row-label event-row-label-status-' + (status || 'unreviewed');
      lbl.textContent = labelText(status);
    }
  }

  function patchTimelineBlockReview(eventId, status) {
    var blk = document.querySelector('.tl-event[data-event-id="' + cssEscape(eventId) + '"]');
    if (!blk) return;
    blk.classList.remove('tl-event-status-snore', 'tl-event-status-not_snore');
    if (status === 'snore') blk.classList.add('tl-event-status-snore');
    else if (status === 'not_snore') blk.classList.add('tl-event-status-not_snore');
  }

  function patchEvent(eventId, status) {
    return patchJSON('/events/' + encodeURIComponent(eventId), { review_status: status });
  }

  function openRetryModal(message) {
    $('#retry-message').textContent = message;
    $('#retry-modal').classList.remove('modal-hidden');
  }

  function closeRetryModal() {
    $('#retry-modal').classList.add('modal-hidden');
  }

  function doRetry() {
    var p = state.pendingPatch;
    if (!p) { closeRetryModal(); return; }
    closeRetryModal();
    setReview(p.status);
  }

  function cancelRetry() {
    state.pendingPatch = null;
    closeRetryModal();
  }

  // ----------------------------------------------------------------
  // Misc UI bits
  // ----------------------------------------------------------------

  var notificationTimer = null;
  function showNotification(text) {
    var b = $('#banner');
    b.className = 'banner banner-banner-warn';
    b.textContent = text;
    if (notificationTimer) clearTimeout(notificationTimer);
    notificationTimer = setTimeout(function () { renderStatus(); }, 1800);
  }

  function cssEscape(s) {
    if (window.CSS && window.CSS.escape) return window.CSS.escape(s);
    return String(s).replace(/[^a-zA-Z0-9_\-]/g, function (c) { return '\\' + c; });
  }

  function refreshAll() {
    pollStatus();
    if (state.currentDate) loadDay();
    else loadDays();
  }

  // ----------------------------------------------------------------
  // Boot
  // ----------------------------------------------------------------

  function bindUI() {
    $('#refresh-btn').addEventListener('click', refreshAll);
    $('#today-btn').addEventListener('click', function () { selectDate(todayLocalDate()); });
    $('#review-snore').addEventListener('click', function () { setReview('snore'); });
    $('#review-not').addEventListener('click', function () { setReview('not_snore'); });
    $('#review-clear').addEventListener('click', function () { setReview('unreviewed'); });
    $('#retry-retry').addEventListener('click', doRetry);
    $('#retry-cancel').addEventListener('click', cancelRetry);

    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) pollStatus();
    });
  }

  function boot() {
    bindUI();
    resetPlayer();
    pollStatus().then(function () { renderStatus(); });
    startStatusPolling();
    loadDays();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();