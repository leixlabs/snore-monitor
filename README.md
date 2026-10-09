# snore-monitor

A Raspberry Pi 3B snore monitor. A USB microphone is captured continuously,
stored as lossless WAV, and analysed by a rule-based detector that marks candidate
snore events. Recordings are kept under a size budget with oldest-first reclamation,
and the events are meant to be reviewed in a browser.

**Events are candidates, not a diagnosis.** The detector is a first-version rule
set (band energy plus noise-floor margins, no machine learning). Its thresholds
must be calibrated with the actual microphone in the actual bedroom; use
`detect-wav` (below) for that. No accuracy claim is valid without real labelled
nights — see "What is implemented" for the current state.

## Threat model — read this first

The HTTP API and Portal have **no authentication and no transport encryption**.
Anything that can reach the port can list recordings, download audio and change
event labels. Therefore:

- The default `server.bind_address` is `127.0.0.1`. Set it to the Pi's LAN address
  (or `0.0.0.0`) only when you need browser access.
- Run it only on a trusted private network. **Never port-forward, reverse-proxy or
  otherwise expose this service to the public internet.**
- Anyone on the same LAN can read the audio. Treat the recordings as private
  medical-adjacent data: they contain a person sleeping, and in a shared network
  (dormitory, guest Wi-Fi, office) that is a real disclosure.
- The recordings and SQLite database are as sensitive as the network. They are
  owned by the `snore-monitor` user with `UMask=0027`; do not copy them to shared
  storage, and remember to wipe the storage before disposing of it.
- The service itself runs unprivileged with `NoNewPrivileges=yes` and no write
  access outside its database and recording directories.

A future revision may add a shared token (`T0.1` leaves this open); until then,
treat LAN access as the entire authentication story.

## What is implemented

Complete and tested:

| Module | Purpose |
|---|---|
| `config` | TOML loading plus all TECH_SPEC §11 validation |
| `dsp`, `resampler`, `detector` | The detection chain: filtering, framing, noise floors, candidate state machine |
| `bounded_queue` | SPSC queues for the audio and control paths |
| `storage`, `recorder`, `retention`, `db_writer` | SQLite schema, WAV writing and recovery, capacity reclamation, async writes |
| `timeline`, `audio_capture`, `metrics` | Clock/gap mapping, ALSA negotiation, status counters |
| `dispatcher` | Frame counting, segmentation, gap registration, fan-out to the recorder and detector workers |
| `http_server`, `web/` | REST API (including Range audio) and the browser Portal |
| `recovery` | Startup scan that repairs `.partial` files and reconciles rows with files |
| `src/main.rs` | Runtime assembly: starts the pipeline, serves HTTP, shuts down in the documented order |
| `src/bin/detect-wav.rs` | Offline calibration tool |

Verified by `cargo test` (134 tests) and `cargo clippy --all-targets`; the binary has
been exercised end to end with a real config, HTTP requests and a SIGTERM shutdown.

What is deliberately left for the target hardware: ALSA device negotiation against a
real USB microphone, the 8-hour run, and threshold calibration on real snoring. This
development machine exposes only an F32 virtual input, which the spec does not accept,
so the service correctly starts in its documented no-microphone state here instead of
pretending to record. See `## Status on a real Pi` at the end of this file.

## Hardware and OS assumptions

- **Board:** Raspberry Pi 3B (ARMv7 or ARM64 depending on the installed OS image).
  The performance budget assumes this class of board.
- **Microphone:** a USB audio capture device. Only `S16_LE` is used, with 1–2
  channels and a capture rate of 16, 32, 44.1, 48 or 96 kHz. Anything else fails
  at startup rather than being converted silently.
- **OS:** Raspberry Pi OS (Debian-based) with ALSA and systemd. The `cpal` crate
  needs the ALSA development headers to build and `libasound2` at runtime.
- **Storage:** a separate USB SSD/flash device is strongly recommended for the
  recordings; a night at 16 kHz mono 16-bit is roughly 115 MB, and the default
  budget is 20 GiB.

## Build

On a development machine:

```sh
sudo apt-get install -y build-essential pkg-config libasound2-dev
cargo build --release
cargo test
```

On the Pi itself (simplest, and what the acceptance run uses): install the same
packages, install Rust with `rustup`, and run `cargo build --release`. Building on
the board avoids every cross-compilation pitfall.

Cross-compiling from a faster machine:

```sh
# One-time setup
rustup target add armv7-unknown-linux-gnueabihf        # or aarch64-unknown-linux-gnu
cargo install cross --git https://github.com/cross-rs/cross

# Build
cross build --release --target armv7-unknown-linux-gnueabihf
```

Do not enable `target-cpu=native`: the resulting binary must run on the target Pi,
not on the build host (TECH_SPEC §10). `release` uses `strip`, and the optimal
`opt-level` is chosen from the T8.2 measurements.

## Install

```sh
# 1. Unprivileged service account (no login shell, no home login)
sudo useradd --system --home /var/lib/snore-monitor --shell /usr/sbin/nologin snore-monitor

# 2. Binary
sudo install -Dm755 target/release/snore-monitor /usr/local/bin/snore-monitor

# 3. Directories
sudo install -d -o snore-monitor -g snore-monitor -m 750 /var/lib/snore-monitor
sudo install -d -m 755 /etc/snore-monitor
# The recordings directory must live on the mounted storage volume, not on the
# SD card. Adjust the path to match your mount point.
sudo install -d -o snore-monitor -g snore-monitor -m 750 /mnt/snore-monitor/recordings

# 4. Configuration
sudo install -m 640 -o root -g snore-monitor config.example.toml /etc/snore-monitor/config.toml
sudoedit /etc/snore-monitor/config.toml    # set both paths and bind_address

# 5. Service
sudo install -Dm644 systemd/snore-monitor.service /etc/systemd/system/snore-monitor.service
sudo systemctl daemon-reload
sudo systemctl enable --now snore-monitor
```

The configuration file is intentionally read-only to the service account: it is
read once at startup, so the service never needs to write it, and a compromised
service cannot alter its own limits.

Storage layout:

| What | Where |
|---|---|
| Configuration | `/etc/snore-monitor/config.toml` (override with `SNORE_MONITOR_CONFIG`) |
| SQLite database | `storage.database_path` — default `/var/lib/snore-monitor/app.db` (plus `-wal`/`-shm`) |
| Recordings | `storage.recording_dir` — default `/mnt/snore-monitor/recordings`, laid out as `recordings/YYYY/MM/DD/<UTC-start>_<segment-id>.wav` |
| Logs | journald (`journalctl -u snore-monitor`) |

During recording a segment exists as `<name>.wav.partial`; it is atomically
renamed to `.wav` only after the header is patched and `fdatasync` succeeds.

## Upgrade

```sh
cd /path/to/snore-monitor
cargo build --release                 # or: cross build --release --target ...
sudo systemctl stop snore-monitor
sudo install -Dm755 target/release/snore-monitor /usr/local/bin/snore-monitor
# Diff the example against your file first: new keys are added over time and an
# unknown key fails startup rather than being ignored.
diff -u /etc/snore-monitor/config.toml config.example.toml || true
sudo systemctl start snore-monitor
journalctl -u snore-monitor -f
```

After upgrading, confirm the service actually records: watch one segment appear
and grow, then let it close. The database schema migration is automatic and
idempotent (`PRAGMA user_version`), but take a copy of the database first if you
care about existing events:

```sh
sudo -u snore-monitor sqlite3 /var/lib/snore-monitor/app.db ".backup /tmp/app.db.bak"
```

Because the configuration rejects unknown keys, an upgrade that adds a key fails
loudly at startup with the key named — it never silently ignores your settings.

## Running under systemd

```sh
sudo systemctl status snore-monitor      # state, restart count, last log lines
sudo systemctl restart snore-monitor
sudo journalctl -u snore-monitor -n 200 --no-pager
sudo journalctl -u snore-monitor -f      # follow live
sudo systemctl cat snore-monitor         # effective unit, including config path
```

The unit runs as `snore-monitor` (never root) with `Restart=on-failure`,
`UMask=0027`, stdout/stderr in the journal, and the configuration path passed
explicitly through `SNORE_MONITOR_CONFIG`.

### USB storage is not mounted

If the recordings live on USB storage, a boot where that disk is missing must not
end with a night of audio written to the SD card. The unit therefore refuses to
start in that situation rather than choosing for you:

- `RequiresMountsFor=/mnt/snore-monitor` makes systemd pull in the mount unit and
  order the service after it.
- `ExecStartPre=/usr/bin/mountpoint --quiet /mnt/snore-monitor` fails the start if
  the path is not a mount point (this also catches the case where a failed mount
  left an empty directory on the root filesystem).

Both paths assume `/mnt/snore-monitor`; change them together with
`storage.recording_dir` if you use another location. The result of this choice is
that a missing disk means **no recording at all** and a failing unit — visible in
`systemctl status` and the journal — instead of silent data loss. If you would
rather keep recording to the SD card as a fallback, that is a deliberate change to
the unit, not the default.

## Offline calibration with `detect-wav`

`detect-wav` replays a WAV file through the *same* detection pipeline the live
service uses (identical resampler, filters, noise floors, state machine), so
tuning thresholds against a recording gives the same events the service would
store. It needs no microphone, no database and no running service.

```sh
cargo build --release --bin detect-wav

# Events as JSON (default) on stdout
detect-wav night.wav

# CSV for spreadsheets, written to a file
detect-wav night.wav --format csv -o events.csv

# Per-frame decision inputs on stderr (levels, floors, thresholds, hit flag)
detect-wav night.wav --frames > events.json 2> frames.csv

# Per-second summary instead of per-frame
detect-wav night.wav --frames-per-second

# Calibrate: start from the deployed configuration, then tighten thresholds
detect-wav night.wav --config /etc/snore-monitor/config.toml \
  --band-ratio-min 0.45 --noise-margin-db 10 --band-margin-db 8

# Two-channel recording: the detector listens to channel 1
detect-wav stereo.wav --channels 2 --mono-channel 1

# Reproduce hourly segment rotation, or a mid-file gap
detect-wav night.wav --mode segments --chunk-seconds 3600
detect-wav night.wav --mode chunks --chunk-seconds 60 --boundary-end-reason segment-end
```

Input must be 16/32/44.1/48/96 kHz, 1–2 channel, `WAVE_FORMAT_PCM` `S16_LE`;
unsupported files fail with an explicit reason. Both arbitrary recordings and the
segments written by the recorder work, including a segment left `.partial` by a
crash (the truncated tail is reported and the samples that exist are analysed).

Useful flags:

| Flag | Effect |
|---|---|
| `--format json\|csv`, `-o FILE` | Output encoding and destination |
| `--frames[=DBFS]`, `--frames-per-second[=DBFS]` | Feature dump on stderr; `DBFS` sets the level above which a frame counts as "over level" |
| `--config PATH` | Start from a deployed configuration instead of the built-in defaults |
| `--band-low-hz`, `--band-high-hz`, `--band-ratio-min`, `--noise-margin-db`, `--band-margin-db`, `--absolute-floor-dbfs` | Detection thresholds |
| `--attack-window-frames`, `--attack-min-hits`, `--hangover-frames`, `--min-event-ms`, `--merge-gap-ms`, `--max-event-seconds` | Onset, hangover and merging behaviour |
| `--noise-floor-window-seconds`, `--noise-floor-percentile`, `--noise-floor-min-seconds`, `--noise-floor-reset-gap-seconds`, `--noise-freeze-max-seconds`, `--filter-warmup-ms` | Noise-floor estimator |
| `--mode continuous\|segments\|chunks`, `--chunk-seconds` | Split the file as a segment rotation or as a gap |
| `--start-time-utc RFC3339`, `--segment-id ID` | Origin written into the event time columns |
| `--mono-channel INDEX`, `--channels 1\|2` | Channel selection; the WAV header stays authoritative |

Overrides are validated with the service's own validator, so a threshold the
service would reject is rejected here too, before any audio is processed. `--help`
lists every flag.

The JSON output is one document with the file's metadata (`capture_rate_hz`,
`channels`, `selected_channel`, `frames`, `duration_seconds`, `truncated`,
`signal_state`, `clipped_samples`, the current noise floors, `events_discarded_short`,
the effective `overrides` and `detector_version`) plus an `events` array. Each
event carries `start_offset_frames`/`end_offset_frames` in stored-input frames,
`duration_ms`, `started_at_utc`, `rule_score`, the feature summary
(`peak_dbfs`, `mean_level_dbfs`, `mean_band_ratio`, `noise_floor_dbfs`),
`end_reason` and `continued`. A value the detector could not compute is emitted as
`null`, never as 0. `--verbose` adds the database-only fields (`segment_id`,
`ended_at_utc`, `detector_version` per event).

A calibration loop looks like this: run `--frames` over a night, look at the rows
around a known false positive, adjust `--noise-margin-db` or `--band-ratio-min`,
and re-run until the candidate list matches what you hear. Write the accepted
values back into `config.example.toml` and bump `detector_version` (T8.6).

## HTTP API and Portal

The service serves the Portal from the installed `web/` directory on
`server.bind_address:server.port` (default `127.0.0.1:8080`), with the API under
`/api/v1`. Endpoints: `/status`, `/days`, `/segments`, `/segments/{id}/gaps`,
`/events`, `/events/{id}` (GET and PATCH), and `/segments/{id}/audio` (supports
`Range`). Keep the threat model above in mind before changing the bind address.

To check a running service:

```sh
curl -s http://127.0.0.1:8080/api/v1/status | python3 -m json.tool
```

A field reading `unknown` or `null` means the value genuinely is not known (for
example before the clock has synced, or while no microphone is attached); the
service never substitutes a plausible-looking value.

## Troubleshooting

### Is the microphone present and usable?

```sh
arecord -l                       # capture devices and their card/device numbers
arecord -L                       # the same devices by name
cat /proc/asound/cards

# What the device really supports (formats, rates, channels)
arecord -D hw:1,0 --dump-hw-params /dev/null

# Record a short sample and play it back
arecord -D hw:1,0 -f S16_LE -r 16000 -c 1 -d 3 /tmp/test.wav
aplay /tmp/test.wav
```

If `arecord -l` shows several capture devices, set `audio.alsa_device` explicitly
(`hw:CARD,DEV`, e.g. `hw:1,0`); an empty value means "there is exactly one" and
startup fails when that is not true.

### The service does not start

The usual causes, in order:

```sh
journalctl -u snore-monitor -n 50 --no-pager
systemctl cat snore-monitor | grep SNORE_MONITOR_CONFIG   # is the right file passed?
```

- **Configuration rejected**: the message names the offending key and lists every
  failure at once. Unknown keys fail too — a typo is never ignored.
- **USB storage not mounted**: `ExecStartPre=/usr/bin/mountpoint` failed; see
  "USB storage is not mounted" above.
- **Multiple or missing capture devices**: set `audio.alsa_device`.
- **Permission denied on the database or recordings**: check ownership, e.g.
  `sudo -u snore-monitor test -w /var/lib/snore-monitor` and the same for the
  recordings directory.
- **Rate unsupported by the device**: the service logs what the device offers and
  which rate was negotiated; the configured value is never silently changed.

### Disk usage and reclamation

```sh
df -h /mnt/snore-monitor                          # free space on the storage
sudo du -sh /mnt/snore-monitor/recordings         # total recordings size
du -sh /mnt/snore-monitor/recordings/*/*/*/*      # size of each day
find /mnt/snore-monitor/recordings -name '*.wav.partial'          # unfinished
find /mnt/snore-monitor/recordings -name '*.wav' -size +2G        # oversized segments
ls -l /var/lib/snore-monitor/app.db*              # database plus WAL sidecars
```

Reclamation deletes the oldest **complete** segments until the total is at or
below `storage.recording_cleanup_target_bytes`, and never deletes the segment
being recorded. Event rows are kept, so a deleted night still shows its events and
is marked as expired. If it cannot get under
`storage.recording_max_bytes`, or free space drops below
`storage.recording_reserve_bytes` and cleanup does not help, the service stops
recording and raises a warning rather than filling the disk.

### Recovering a `.partial` file

A `.partial` is a segment interrupted by a crash, a kill, or a shutdown timeout.
Its header still says "0 frames"; the audio after the 44-byte header is intact.
The startup scan truncates it to a whole frame, patches the header, renames it to
`.wav` and marks it `interrupted` in the database, so it plays normally with a
shorter tail. To do this by hand for one file:

```sh
# 1. Inspect: rate, channels and the declared (wrong) data length
xxd -l 64 recording.wav.partial

# 2. Truncate to a whole number of frames of a 44-byte-header PCM file, e.g.
#    16 kHz mono 16-bit = 2 bytes per frame:
size=$(stat -c %s recording.wav.partial)
data=$(( (size - 44) / 2 * 2 ))
truncate -s $(( 44 + data )) recording.wav.partial

# 3. Patch the two RIFF lengths (offsets 4 and 40) and rename. `sox` is the
#    least error-prone way to rewrite a WAV header:
sox --ignore-length recording.wav.partial recovered.wav
```

Only do this on a **copied** file if you intend to keep the original. The service
must not be running on that directory while you rewrite files.

### Verifying audio and event integrity

```sh
# Playable and sane length?
ffprobe -hide_banner recording.wav
soxi recording.wav

# Timestamps are stored as RFC 3339 UTC text; grouping on the date part keeps the
# grouping unambiguous regardless of the stored offset.
# How many events per night?
sudo -u snore-monitor sqlite3 /var/lib/snore-monitor/app.db \
  "select substr(started_at_utc, 1, 10) as day, count(*) from snore_events group by day order by day;"

# Gaps recorded per segment, and how many frames were actually lost
sudo -u snore-monitor sqlite3 /var/lib/snore-monitor/app.db \
  "select segment_id, count(*), coalesce(sum(lost_frames), 0) from audio_gaps group by segment_id;"

# Segments that never completed (interrupted recordings)
sudo -u snore-monitor sqlite3 /var/lib/snore-monitor/app.db \
  "select id, status, frame_count, size_bytes from recording_segments where status <> 'complete';"

# Database consistency and free space
sudo -u snore-monitor sqlite3 /var/lib/snore-monitor/app.db "pragma integrity_check;"
```

Any clip playback offset should be checked against the same gap table; the Portal
is specified to map offsets through `audio_gaps` rather than subtracting UTC
timestamps, because that mapping is what stays correct across a gap or a clock
step.

### Known limitations to keep in mind while debugging

- Detection is suspended while it is warming up (no reliable noise floor yet, by
  default the first 10 s) and for the filter warm-up after a reset. A short burst
  early in a night is expected to be missed.
- Events are finalized about `merge_gap_ms` (default 800 ms) after the last hit,
  so the most recent candidate is briefly not in the database yet.
- A candidate is force-closed by a gap, a segment boundary, or shutdown; those
  events carry `end_reason` of `gap`, `segment_end` or `shutdown`.
- There is no real-time clock on a Pi without one attached, so the first seconds
  after a network-less boot can be `unsynced`; timestamps are corrected when NTP
  syncs, and the segment rotates at that step.

## Development

```sh
cargo test                  # unit and integration tests, no hardware needed
cargo clippy --all-targets  # lints
cargo fmt                   # formatting
```

Every DSP stage is testable without a microphone: `tests/resampler_metrics.rs`
measures the TECH_SPEC §5.1 passband and alias-rejection numbers, and the detector
unit tests cover the state machine, noise floors and offset mapping. `detect-wav`
is the bridge from synthetic fixtures to real recordings, and its own tests build
their WAV fixtures in a temporary directory rather than committing binary files.

## Reference

- `TECH_SPEC.md` — the authoritative design (DSP, storage, API, verification thresholds).
- `tasks.md` — the work breakdown, including which acceptance items still require
  a real Raspberry Pi 3B.
- `config.example.toml` — every configuration key with its documented default.

## Status on a real Pi

Nothing below can be established without the target board; do not treat the
service as validated until these are done (T8.x):

- ALSA negotiation against the actual USB microphone, including which rate and
  channel count it settles on, and the fallback path when the first choice is
  unavailable.
- Whether CPAL/ALSA supplies reliable capture timestamps here. If it does not,
  gap detection degrades to counters and `/status` reports
  `gap_detection_limited: true`.
- An 8-hour run: RSS under 80 MiB, DSP under 10% of one core on average.
- Clock behaviour with no RTC: `time_quality` staying `unsynced` until NTP settles,
  then segments rotating and being corrected.
- The `systemd/` unit, including what happens when the USB storage is not mounted.
- Threshold calibration on real snoring (T8.6). Do not derive accuracy from
  synthetic audio.
