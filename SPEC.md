# pitpat-exporter — Specification

Control a PitPat walking-pad treadmill over Bluetooth LE from a NixOS / KDE Plasma 6 desktop,
and export its data as Prometheus metrics.

- **Author / copyright:** Lapo Luchini <lapo@lapo.it>
- **License:** ISC (full text in `LICENSE`, `license = "ISC"` in `Cargo.toml`, SPDX header
  `// SPDX-License-Identifier: ISC` on every source file)
- **Protocol reference:** <https://github.com/KeiranY/PitPat-WebBT> (`treadmill.js`); everything
  needed is restated in §3, so the reference is only needed for cross-checking.

---

## 1. Goals and non-goals

### Goals
1. A background service (`pitpat daemon`) that owns the single BLE connection to the treadmill,
   keeps it alive, reconnects automatically, and exposes:
   - Prometheus metrics (`/metrics`), most importantly **the distance walked**, both for the
     current session and as a lifetime counter;
   - a small JSON control API;
   - a tiny built-in web page (usable from a phone as well).
2. A **Plasma 6 widget** (plasmoid, pure QML) for the panel / system tray, as the main GUI.
3. A CLI (`pitpat status|start|pause|stop|speed …`) for scripting and KDE global shortcuts.
4. A **simulator** mode so everything except the real BLE link can be developed and tested
   without hardware.
5. Nix flake with package(s) and a NixOS module.
6. Very low RAM: the daemon must stay **below 15 MiB RSS** when connected and idle; the GUI adds
   no process of its own (it runs inside `plasmashell`).

### Non-goals (YAGNI)
- No incline control (walking pads don't have one; the protocol byte stays 0).
- No MQTT / Home Assistant, no multi-treadmill support, no user accounts.
- No standalone Qt/GTK window application.
- No mph *input* mode: commands are always sent in km/h mode (see §3.3); mph *readings* are
  converted (see §3.2).

---

## 2. Architecture

```
            ┌──────────────────────────── pitpat daemon (Rust, 1 process) ───────────────────────────┐
 BlueZ ◄──► │ Link (trait)  ──events──►  Core state machine  ──►  Sessions + lifetime totals (disk) │
 (D-Bus)    │  ├ BleLink (bluer)            │  ▲ commands                                            │
            │  └ SimLink (--simulate)       ▼  │                                                     │
            │                         HTTP server (axum): /metrics /api/* / (web page)             │
            └──────────────────────────────────────▲───────────────────────────────────────────────┘
                                                   │ HTTP/JSON (localhost)
                     ┌─────────────────────────────┼─────────────────────────────┐
              Plasma 6 widget (QML)        pitpat CLI (same binary)       browser / Prometheus
```

- The daemon is a **single binary** `pitpat` with subcommands (`daemon`, `status`, `start`, …).
- Use a **tokio `current_thread` runtime** (no worker thread pool) to keep RAM down.
- `Link` is a trait with two implementations, so the core, HTTP and metrics code can be tested
  with the simulator and with scripted fake links in unit tests.

### 2.1 Suggested source layout

```
Cargo.toml  Cargo.lock  LICENSE  README.md  SPEC.md  flake.nix  flake.lock
src/
  main.rs        # clap entry point, subcommand dispatch
  config.rs      # CLI flags / env vars → Config
  protocol.rs    # pure encode/decode, no I/O (unit-tested with the vectors in §3.4)
  link.rs        # Link trait + LinkEvent
  ble.rs         # BleLink: bluer implementation
  sim.rs         # SimLink: fake treadmill
  core.rs        # state machine, command queue, auto-stop, reconnect policy
  session.rs     # session tracking, lifetime totals, persistence
  metrics.rs     # Prometheus registry
  http.rs        # axum routes, auth, Host/CSRF checks
  cli.rs         # client subcommands (talk HTTP to the daemon)
web/index.html   # embedded with include_str!, no external resources
plasmoid/it.lapo.pitpat/
  metadata.json
  contents/ui/main.qml  CompactRepresentation.qml  FullRepresentation.qml  configGeneral.qml
  contents/config/main.xml  contents/config/config.qml
contrib/grafana/pitpat.json      # Grafana dashboard
nix/package.nix  nix/plasmoid.nix  nix/module.nix
```

### 2.2 Rust dependencies (keep the list short)
`tokio` (features: `rt`, `macros`, `sync`, `time`, `signal`, `fs`), `bluer` (feature `bluetoothd`),
`futures`, `axum` (default features off where possible; needs `json`, `query`, `tokio`, `http1`),
`serde`, `serde_json`, `clap` (derive, env), `prometheus-client`, `tracing`, `tracing-subscriber`,
`ureq` (CLI only; small blocking client). Nothing else without a good reason.

Release profile: `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`, `opt-level = "s"`.

---

## 3. Treadmill protocol

### 3.1 GATT

| Item | UUID |
|---|---|
| Service | `0000fba0-0000-1000-8000-00805f9b34fb` |
| Write characteristic (commands) | `0000fba1-0000-1000-8000-00805f9b34fb` |
| Notify characteristic (status) | `0000fba2-0000-1000-8000-00805f9b34fb` |

**Write type:** every write to fba1 must be an ATT **Write Request** (with response). The
characteristic is `[read, write]`, and the treadmill silently ignores Write Commands (without
response): on the user's hardware, commands sent as Write Commands were all ignored. With bluer, use `write_ext` with `WriteOp::Request`:
plain `write()` defaults to `WriteOp::Command`.

**Keep-alive rule:** for **every** notification received, the client writes exactly one packet to
the write characteristic: the next queued command if there is one, otherwise the heartbeat
`6a 05 fd f8 43`. Commands are *only* sent this way (never spontaneously). The notification rate is
whatever the treadmill uses (expected ~1–2 Hz); log it at debug level.

### 3.2 Status notification (treadmill → host)

Packets shorter than 31 bytes are **invalid**: log at debug level, count them in
`pitpat_invalid_packets_total`, do not change state, still reply with heartbeat/command.
All integers are **big-endian, unsigned**. Offsets are byte indices:

| Field | Offset | Size | Meaning |
|---|---|---|---|
| current speed | 3 | u16 | speed × 1000 (km/h or mph, see unit flag) |
| target speed | 5 | u16 | the treadmill's own target, × 1000, same unit |
| distance | 7 | u32 | distance × 1000 (km or mi) → in km mode, **metres** |
| calories | 18 | u16 | kcal, current session |
| duration | 20 | u32 | milliseconds, current session |
| flags | 26 | u8 | see below |
| steps | 43 | u16 | step count, current session — 50-byte `68 32` frames only |

Bytes 14–17 are unused on the model seen on hardware. The frame right
after connect is a 52-byte `68 34` variant with an ASCII serial in its
tail and **no steps value**: the daemon keeps the last count it saw.

Flags byte:
- bit 7 (`0x80`): 1 = imperial (mph / mi), 0 = metric.
- bits 3–4 (`flags & 0x18`): `0x18` = starting, `0x08` = running, `0x10` = paused, `0x00` = stopped.

Normalise everything to SI internally: speed in m/s, distance in metres
(imperial: `value × 1.609344`), duration in seconds (float).

Hardware note (the 52-byte variant seen on real hardware, C4:A9:B8:…):
the frame starts `68 34` and is 52 bytes long, but the field offsets
above are unchanged and the values decoded correctly. Steps were read
from the wrong offset until a Nov 17 hardware trace pinned them at
u16 @43 of the 50-byte `68 32` frame (see the table): the counter
climbs with actual footfalls (86 steps counted vs 86 reported, long
steps 65 vs 66), but very short, light steps are under-counted (114
counted vs 77) — the daemon reports the device value as is. kcal @18
is correct. After a stop the treadmill may reset duration and kcal to
0 while distance drops first; steps behaved inconsistently between
traces (staying at 268 once, resetting the other time).

### 3.3 Command packet (host → treadmill), 23 bytes

| Byte(s) | Value |
|---|---|
| 0 | `0x6a` start byte |
| 1 | `0x17` length (23) |
| 2–5 | `0x00` |
| 6–7 | target speed × 1000, u16 BE, km/h (range 1000–6000) |
| 8 | `0x05` for set-speed, `0x01` for start / pause / stop |
| 9 | incline, always `0x00` |
| 10 | user weight in kg (config `weight_kg`, default 80; presumably used for calories) |
| 11 | `0x00` |
| 12 | command: `0x04` start or set-speed, `0x02` pause, `0x00` stop; bit 3 cleared = km/h mode |
| 13–20 | user ID, u64 BE (config `user_id`, default `58965456623`) |
| 21 | checksum = XOR of bytes 1..=20 |
| 22 | `0x43` end byte |

For pause/stop, the speed field carries the current target speed (any valid value works; the
reference sends 1000; we send the target speed, clamped). A SetSpeed while paused starts the
belt on this model but loses the speed (K8): the daemon sends a Start carrying the requested
speed instead and re-asserts it afterwards (§3.6).

### 3.4 Golden test vectors (weight 80, default user ID)

These must be unit tests in `protocol.rs`:

```
heartbeat           6a 05 fd f8 43
start     2.5 km/h  6a 17 00 00 00 00 09 c4 01 00 50 00 04 00 00 00 0d ba 9d 76 ef 3c 43
set_speed 3.2 km/h  6a 17 00 00 00 00 0c 80 05 00 50 00 04 00 00 00 0d ba 9d 76 ef 79 43
pause     1.0 km/h  6a 17 00 00 00 00 03 e8 01 00 50 00 02 00 00 00 0d ba 9d 76 ef 1c 43
stop      1.0 km/h  6a 17 00 00 00 00 03 e8 01 00 50 00 00 00 00 00 0d ba 9d 76 ef 1e 43
```

Also unit-test the decoder with hand-built 31-byte packets for each state and both unit modes,
plus a 30-byte packet (→ invalid).

### 3.5 Command queue semantics
The reference implementation holds one pending packet and silently drops older ones (so e.g.
"start" followed quickly by "+0.1" loses the start). Instead:
- Keep a small FIFO (capacity 8; reject with HTTP 503 when full).
- Consecutive **set-speed** commands are coalesced (the newer one replaces the queued one).
- **Stop** clears the queue and goes to the front: stop must never wait behind anything.
- If a write fails, retry the same packet on the next notification, at most 3 times, then drop it
  and log a warning.

### 3.6 Speed rules
- Allowed range: `min_speed`..=`max_speed` km/h (defaults 1.0 and 6.0), step 0.1, values clamped.
- Speed up and speed down both use a step of **0.1 km/h** (the reference has an asymmetric
  +0.1/−1.0 bug; don't copy it).
- The daemon remembers `target_speed`: initialised from config `default_speed` (2.0), updated by
  every set-speed command, and used by "start". When the treadmill reports *running*, sync
  `target_speed` to the reported speed once the belt has settled (i.e. ignore the ramp-up while
  starting) so that physical-remote changes are reflected.
- Hardware quirk (F1/K8, refined by the Nov 17 traces): when started from stopped, the belt
  **ignores the speed field of the start command** and ramps to its own default (1.0); when a
  `SetSpeed` starts it from paused, it also loses the speed. The daemon therefore re-asserts:
  after a `Start(v)` or a speed-set while paused (the daemon sends `Start(v)` itself, one code
  path), remember `v`; on `running` packets, if the treadmill's target (@5) isn't `v`, re-queue
  `SetSpeed(v)` (timed retries, at most 3), and suppress the settle-sync until the belt reports
  `v` (at most ~10 s), so the remote pickup doesn't erase the user's speed. Resume from paused
  honours the Start speed and the reassertion stays silent.

---

## 4. Daemon behaviour

### 4.1 Connection
- Adapter: config `adapter` (default: BlueZ default adapter).
- Device: if config `address` is set, connect to it directly (discovering if not yet known to
  BlueZ); otherwise scan for devices advertising service `fba0` and use the first one found. Log
  the address so the user can pin it.
- No pairing/bonding is required (the web version doesn't pair).
- On disconnect or any BLE error: state becomes `disconnected`, end the open session (§4.3), and
  reconnect with exponential backoff 1 s → 2 s → … capped at 30 s, forever. Count reconnects in
  `pitpat_reconnects_total`.
- Liveness watchdog: if no notification has arrived for 10 s while connected, force a disconnect
  and reconnect.

### 4.2 States
`disconnected | stopped | starting | running | paused`, exposed in the API and metrics.

### 4.3 Sessions
- A session **starts** on the first `running` after `stopped`/`disconnected`.
- `paused` does **not** end a session (the reference does; that splits one walk into many rows).
- A session **ends** on `stopped`, on disconnect, or on daemon shutdown.
- Session record: `start` (RFC 3339), `end`, `distance_m`, `steps`, `kcal`, `duration_s`
  (treadmill-reported moving time), `avg_speed_kmh` (= distance / duration), `max_speed_kmh`,
  `end_reason` (`stopped|disconnected|shutdown|target_reached`).
- The record uses the last values reported **before** the treadmill reset its counters (counters
  appear to go back to 0 on stop).
- Completed sessions are appended as one JSON object per line to `$STATE_DIR/sessions.jsonl`.
  Sessions shorter than 10 s **and** under 10 m are discarded as noise.

### 4.4 Lifetime totals (the "track the walked distance" requirement)
- Lifetime counters: distance (m), steps, kcal, moving seconds, sessions.
- Update them with **deltas**: `delta = new - last` when `new >= last`; when the treadmill counter
  goes **down** (reset), treat `new` itself as the delta (counting from 0). Ignore deltas from the
  first packet after (re)connecting if the session is a continuation of one already counted
  (i.e. the session counters didn't reset) to avoid double counting. Reject absurd deltas
  (> 50 m or > 100 steps between two packets) and log a warning.
- Persist to `$STATE_DIR/totals.json` atomically (write temp file + rename) at session end, every
  60 s while running, and on shutdown. Load at startup.

### 4.5 Targets (auto-stop)
- Optional `target_distance_m` and/or `target_duration_s`, set via API/CLI/widget, cleared
  after triggering or via `DELETE /api/target`.
- They apply to the current/next session. When reached, queue **stop**, end the session with
  `end_reason = target_reached`, and clear the target.
- Targets live in memory only (not persisted).

### 4.5a Idle sleep (on-demand connection)
While a BLE link is up, the treadmill beeps on connect and keeps its LCD lit. So the daemon holds
the link only while it's needed.

- **Sleep:** when the belt has been `stopped` for `idle_timeout` (config, default `5m`; `0` keeps
  the old always-connected behaviour) with no control API call in that time, disconnect cleanly
  (`device.disconnect()`) and enter state **`sleeping`**. While sleeping there are no reconnect
  attempts, no backoff and no watchdog. `sleeping` is not an error; `disconnected` keeps meaning
  "wanted a link and don't have one".
- **Start-up:** the daemon starts in `sleeping`, so a reboot doesn't make the treadmill beep.
  Exception: `idle_timeout = 0`.
- **Link lost:** if the link drops (or the watchdog fires) while the belt is `stopped`, go straight
  to `sleeping`: the treadmill was most likely switched off, and reconnecting forever would keep the
  radio busy. If it drops mid-walk (a session is open), handle the loss as in §4.1 (the session
  closes as `disconnected` right away, state `disconnected`) and reconnect with backoff, but give up
  after `idle_timeout` without a link and sleep. `POST
  /api/sleep` also ends such a reconnect loop. With `idle_timeout = 0` the old reconnect-forever
  behaviour stays.
- **No scanning on wake:** with `--address`, waking and reconnecting never start an LE discovery
  (see §4.1 and the RTL8761BU note): connect to the known device, or, if BlueZ has forgotten it, use
  `Adapter1.ConnectDevice` (bluer `connect_device`, BlueZ experimental API), which connects to an
  address without a scan.
- **Wake:** any control call that needs the belt wakes the daemon: start, pause, toggle, stop, and
  the new `POST /api/connect` (CLI `pitpat connect`). Speed changes (absolute, delta or preset)
  while sleeping only update the remembered target speed, as in `stopped` (§3.6), and do **not**
  wake: a stray mouse-wheel over the panel icon must not make the treadmill beep. Waking means connecting as in
  §4.1, then running normally. The command that caused the wake is queued and sent on the first
  notification cycle, as usual. If the connection doesn't come up within 30 s, drop the queued
  command and go back to `sleeping`. Return 202 as usual; the result shows up in `/api/state`.
  Target set/clear does **not** wake.
- `POST /api/sleep` (CLI `pitpat sleep`) disconnects now. It is refused with 409 while the belt is
  `starting`, `running` or `paused`.
- **Late-connect crediting:** the daemon only sleeps after `stopped`, so a belt found `running` or
  `paused` at wake-up is a walk the daemon hasn't counted yet; it was started with the remote. Open
  the session with the treadmill's current counters as its starting values. Add those counters to
  the lifetime totals once, bypassing the per-packet absurd-delta check, but cap them at 20 km /
  30 000 steps and log a warning if exceeded.
- **Remote-started walks while sleeping:** see 4.5b. Without 4.5b, a walk started and finished with
  the remote while the daemon sleeps is not recorded. Document that in the README.
- Widget and web page: show `sleeping` as "asleep". The popup gets a **Connect** button. Opening the
  popup does **not** wake the daemon, so glancing at the widget doesn't make the treadmill beep.
  Start, stop, preset and speed controls stay enabled while sleeping (start and stop wake it; speed
  and preset set the next start's speed).
- Metrics: `pitpat_state` gains `sleeping`, and `pitpat_connected` is 0 while sleeping.

### 4.5b Advertisement watch while sleeping: dropped for now
The idea was to watch the treadmill's advertisements while `sleeping`, with no connection and no
beep, and wake when the belt starts. It would need **passive** scanning through BlueZ's
AdvertisementMonitor API, never discovery. On the user's controller (Realtek RTL8761BU) every scan
attempt crashed the firmware, including a raw passive scan (`LE Set Scan Parameters` timed out, then
a USB reset). So this can't be built or verified there. Don't implement it. Revisit only if the user
changes Bluetooth hardware.

### 4.6 Shutdown
On SIGTERM/SIGINT: if the treadmill is running or paused, send **stop** (wait up to 3 s for it to
go out via the notification cycle), close the session, persist totals, disconnect, exit 0.

### 4.7 Simulator (`--simulate`)
`SimLink` emits a notification every 500 ms. It reacts to the commands like a real treadmill:
start → `starting` for 3 s → `running` with the speed ramping at 0.5 km/h per second to the
**belt's own default (1.0)** — like the hardware, it ignores the start command's speed field, so
the daemon's corrective `SetSpeed` is exercised; pause/stop as expected; counters advance from
the speed (steps ≈ distance / 0.7 m; kcal ≈ 0.5 × weight_kg × km); stop resets counters to 0.
The status packets carry the treadmill's own target speed at offset 5. It encodes real 31-byte packets so that
the decoder path is exercised too. It must also be able to simulate a disconnect
(`SIGUSR1` → drop the link) to test reconnect logic by hand.

---

## 5. Configuration

All options are CLI flags of `pitpat daemon`, each also readable from an env var `PITPAT_<NAME>`.
No config file.

| Flag | Default | Notes |
|---|---|---|
| `--listen` | `127.0.0.1:9839` | HTTP listen address |
| `--token-file` | – | file holding a shared secret; **required** if `--listen` is not loopback (refuse to start otherwise) |
| `--adapter` | BlueZ default | e.g. `hci0` |
| `--address` | – | treadmill MAC |
| `--state-dir` | `$STATE_DIRECTORY` or `$XDG_STATE_HOME/pitpat` | sessions.jsonl, totals.json |
| `--weight-kg` | `80` | 20–250 |
| `--user-id` | `58965456623` | u64 |
| `--min-speed` / `--max-speed` | `1.0` / `6.0` | km/h |
| `--default-speed` | `2.0` | km/h |
| `--presets` | `2,3,4` | km/h, up to 6 values, shown in GUIs |
| `--simulate` | off | use SimLink |
| `--log` | `info` | tracing filter |

The client subcommands accept `--url` (default `http://127.0.0.1:9839`, env `PITPAT_URL`) and
`--token-file` (env `PITPAT_TOKEN_FILE`).

---

## 6. HTTP interface

### 6.1 Security
The API can start a treadmill someone may be standing on, so:
- Default bind is loopback only.
- **Mutating requests** (`POST`, `DELETE`) must have `Content-Type: application/json` (forces a
  CORS preflight from browsers; never send CORS allow headers) and, if a token is configured,
  `Authorization: Bearer <token>` (compare in constant time).
- Check the `Host` header against `localhost`, `127.0.0.1`, `[::1]` and the configured listen
  address (prevents DNS-rebinding attacks); otherwise 421.
- `GET` endpoints (state, sessions, metrics, page) are read-only and need no token.
- Bodies are limited to 4 KiB; reject unknown JSON fields; validate all numbers (range, finite).

### 6.2 Endpoints

| Method & path | Body | Result |
|---|---|---|
| `GET /` | – | embedded web page |
| `GET /metrics` | – | Prometheus text format |
| `GET /healthz` | – | `200 ok` (daemon alive, regardless of BLE) |
| `GET /api/state` | – | state JSON (below) |
| `GET /api/events` | – | Server-Sent Events, one `state` event per notification (≤ 2/s) |
| `GET /api/sessions?limit=N` | – | newest-first array of session records (default 20, max 500) |
| `POST /api/start` | `{}` or `{"speed_kmh": 3.0}` | queue start |
| `POST /api/pause` | `{}` | queue pause |
| `POST /api/stop` | `{}` | queue stop (front of queue) |
| `POST /api/speed` | `{"speed_kmh": 3.2}` or `{"delta_kmh": 0.1}` or `{"preset": 1}` | set target speed (queues a set-speed if running) |
| `POST /api/target` | `{"distance_m": 3000}` and/or `{"duration_s": 1800}` | set auto-stop target |
| `DELETE /api/target` | – | clear target |

Commands return `202 {"queued": true}`, `409` if disconnected, `400` on validation errors (JSON
`{"error": "…"}`), `503` if the queue is full.

`/api/state` example:

```json
{
  "connected": true,
  "address": "AA:BB:CC:DD:EE:FF",
  "state": "running",
  "imperial": false,
  "speed_kmh": 3.2,
  "target_speed_kmh": 3.2,
  "presets_kmh": [2, 3, 4],
  "min_speed_kmh": 1.0,
  "max_speed_kmh": 6.0,
  "session": { "start": "2026-11-10T09:12:00+01:00", "distance_m": 1234, "steps": 1700,
               "kcal": 52, "duration_s": 1403, "max_speed_kmh": 3.5 },
  "target": { "distance_m": 3000, "duration_s": null },
  "today": { "distance_m": 4520, "steps": 6300, "kcal": 190, "duration_s": 5100 },
  "lifetime": { "distance_m": 812345, "steps": 1100000, "kcal": 35000, "duration_s": 900000,
                "sessions": 312 }
}
```

`session` and `target` are `null` when absent. `today` is the sum of today's finished sessions
plus the open session (local time zone).

---

## 7. Prometheus metrics

Base units, `pitpat_` prefix. Gauges reflect the treadmill's own per-session counters; counters
are the daemon's persisted lifetime totals (monotonic, survive restarts, so `increase()` works).

| Metric | Type | Notes |
|---|---|---|
| `pitpat_connected` | gauge | 0/1 |
| `pitpat_state{state="…"}` | gauge | 1 for the current state, 0 for the others (all 5 always present) |
| `pitpat_speed_meters_per_second` | gauge | current belt speed |
| `pitpat_target_speed_meters_per_second` | gauge | |
| `pitpat_session_distance_meters` | gauge | |
| `pitpat_session_steps` | gauge | |
| `pitpat_session_energy_kilocalories` | gauge | |
| `pitpat_session_duration_seconds` | gauge | |
| `pitpat_distance_meters_total` | counter | **lifetime walked distance** |
| `pitpat_steps_total` | counter | |
| `pitpat_energy_kilocalories_total` | counter | |
| `pitpat_moving_seconds_total` | counter | |
| `pitpat_sessions_total` | counter | |
| `pitpat_reconnects_total` | counter | |
| `pitpat_invalid_packets_total` | counter | |
| `pitpat_last_notification_timestamp_seconds` | gauge | Unix time |
| `pitpat_build_info{version="…"}` | gauge | always 1 |

`contrib/grafana/pitpat.json`: a dashboard with
- current speed and state;
- distance per day (`increase(pitpat_distance_meters_total[1d])`), shown as bars;
- steps per day;
- active minutes per day;
- lifetime km.

It uses a `${datasource}` variable rather than a fixed Prometheus data source.

---

## 8. CLI

```
pitpat daemon [flags]              # §5
pitpat status [--json]             # one-line human summary, or raw /api/state
pitpat start [SPEED_KMH]
pitpat pause
pitpat toggle                      # running → pause; otherwise start
pitpat stop
pitpat speed 3.2 | +0.1 | -0.1 | preset 2
pitpat target [--distance 3km|2500m] [--time 30m|1h15m] | pitpat target clear
pitpat sessions [-n 10]
```

Exit codes: 0 ok, 1 daemon error response (message on stderr), 2 daemon unreachable.

### 8.1 KDE global shortcuts
A plasmoid can't register several global shortcuts, so use the CLI. `README.md` documents how
to bind these in *System Settings → Keyboard → Shortcuts → Add New → Command*:
Meta+Space → `pitpat toggle`, Meta+Up → `pitpat speed +0.1`, Meta+Down → `pitpat speed -0.1`,
Meta+End → `pitpat stop`.

---

## 9. Built-in web page (`web/index.html`)

- One self-contained file: inline CSS/JS, no frameworks, no external URLs. Target size < 15 KiB.
- Mobile-friendly; follows `prefers-color-scheme`.
- Shows: connection/state chip, big current speed, session distance / time / steps / kcal,
  target progress bar, today and lifetime distance.
- Controls: Start/Pause toggle, Stop (visually distinct, always reachable), −0.1 / +0.1, slider
  (min..max step 0.1, sends on release), preset buttons, target distance/time inputs with
  Set/Clear.
- Last 10 sessions table.
- Live updates via `EventSource('/api/events')`, falling back to polling `/api/state` every 2 s.
- If a token is required, ask for it once and keep it in `localStorage`.
- Control buttons are disabled when disconnected or in `starting`.

---

## 10. Plasma 6 widget (`plasmoid/it.lapo.pitpat`)

- Pure QML, Plasma 6 API (`metadata.json` with `"KPackageStructure": "Plasma/Applet"`,
  `"X-Plasma-API-Minimum-Version": "6.0"`, `Id: it.lapo.pitpat`, license ISC, author as above).
  Use `PlasmoidItem`, `PlasmaComponents3`, `Kirigami` per current Plasma 6 conventions.
- Talks to the daemon with `XMLHttpRequest`. Poll `/api/state` every 1 s while the popup is open
  or the belt is moving, otherwise every 10 s.
- **Compact representation** (panel): icon (`org.kde.plasma` icon `"run-build"` or similar
  standard icon; a custom SVG is fine) plus, while a session is open, the session distance
  first (e.g. `1.23 km · 3.2 km/h`). **No config option: the panel thickness picks the level
  of detail** — height in text lines (from `Kirigami.Units.gridUnit`): icon + distance only
  when very thin, `1.23 km · 2.5 km/h` on one line, distance (larger) / speed on two, plus the
  session time (`mm:ss`) on three; the width follows the widest line so nothing is clipped.
  Vertical panels keep the icon only. Distance always comes first. Icon greyed out when
  disconnected. Left click = popup, middle-click = toggle start/pause. Mouse wheel over it =
  ±0.1 km/h.
- **Full representation** (popup): same information and controls as the web page (§9) except the
  sessions table (show only the last 3 sessions), in native Plasma style.
- **Tooltip:** state, speed, session distance, today's distance.
- **Notifications** (via the KF6 `org.kde.notification` QML module; if that turns out not to work
  inside a plasmoid, fall back to running `notify-send` through the `executable` engine):
  each distance milestone (config, default every 1 km of session distance), target reached,
  disconnect while running.
- **Config page** (`main.xml` + `configGeneral.qml`): daemon URL, token, milestone km (0 = off),
  pin (keep the popup open while clicking elsewhere).
- **Popup pin** (`window-pin` tool button in the popup's top row, bound to the `pin` config,
  `hideOnWindowDeactivate: !pin`): while pinned the popup stays open and live-updates during a
  walk.
- When the daemon is unreachable: show "pitpat daemon not running" with a hint, and no errors in
  the log beyond a single warning.

---

## 11. Nix packaging

`flake.nix` (inputs: `nixpkgs` only; systems `x86_64-linux`, `aarch64-linux`):
- `packages.<sys>.pitpat`: `rustPlatform.buildRustPackage` using `cargoLock.lockFile`;
  `nativeBuildInputs = [ pkg-config ]`, `buildInputs = [ dbus ]`; `meta.license = lib.licenses.isc`;
  `meta.mainProgram = "pitpat"`.
- `packages.<sys>.plasmoid`: installs the plasmoid to
  `$out/share/plasma/plasmoids/it.lapo.pitpat`.
- `packages.<sys>.default` = `pitpat`.
- `devShells.<sys>.default`: rust toolchain, rust-analyzer, clippy, rustfmt, pkg-config, dbus.
- `checks.<sys>`: package build (which runs `cargo test`), and a NixOS VM test
  (`nixosTest`) that starts the service with `--simulate` and checks `/healthz`, `/metrics`,
  and that `POST /api/start` makes the state reach `running`.
- `nixosModules.default`.

### 11.1 NixOS module (`services.pitpat`)

```nix
services.pitpat = {
  enable = true;
  package = …;               # default: the flake package
  address = "AA:BB:…";       # null = auto-discover
  adapter = null;
  listen = "127.0.0.1:9839";
  tokenFile = null;          # path, required by assertion if listen is not loopback
  openFirewall = false;      # opens the listen port
  weightKg = 80;
  presets = [ 2 3 4 ];
  simulate = false;
  extraArgs = [ ];
  widget.enable = true;      # adds the plasmoid + pitpat CLI to environment.systemPackages
};
```

The module also:
- sets `hardware.bluetooth.enable = true`;
- installs `pitpat` into `environment.systemPackages`, so the CLI is available for shortcuts;
- defines a **system** service `pitpat.service`:
  - `wantedBy = multi-user.target`, `after`/`wants = bluetooth.service`;
  - `Restart=on-failure`, `RestartSec=5`;
  - `DynamicUser=yes`, `StateDirectory=pitpat`;
  - hardening: `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`,
    `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `CapabilityBoundingSet=` (empty),
    `MemoryMax=64M`;
  - `LoadCredential` for the token file.

  **Verify on real hardware** that a `DynamicUser` may talk to `org.bluez` on the system bus
  (BlueZ's D-Bus policy). If it can't, switch to a static `pitpat` user/group plus a
  `services.dbus.packages` policy snippet that allows that user to talk to `org.bluez`, and
  document why in the module.

Prometheus side (user's own config, documented in README only):
`services.prometheus.scrapeConfigs = [{ job_name = "pitpat"; static_configs = [{ targets = [ "127.0.0.1:9839" ]; }]; }];`

---

## 12. Logging and errors
- `tracing` to stderr (journald picks it up), no timestamps when `JOURNAL_STREAM` is set.
- Info: connect/disconnect, device address, session start/end summary, target reached.
- Debug: every packet in hex, both directions.
- Never panic on bad input from BLE or HTTP. BLE errors trigger reconnect; HTTP errors become
  4xx/5xx JSON.
- Corrupt `totals.json`: rename it to `totals.json.corrupt-<timestamp>`, log an error, start from
  zero. Corrupt lines in `sessions.jsonl` are skipped with a warning.

---

## 13. Repository and process
- Git branch `trunk`. One commit per logical change, English messages that explain *why*.
- `README.md`: what it is, protocol credit to KeiranY/PitPat-WebBT, NixOS usage (flake input +
  module), non-Nix build (`cargo build --release`), shortcuts (§8.1), Prometheus/Grafana snippet,
  safety note about remote control.
- `LICENSE`: ISC, `Copyright (c) 2026 Lapo Luchini <lapo@lapo.it>`.
- `Cargo.toml`: `authors = ["Lapo Luchini <lapo@lapo.it>"]`, `license = "ISC"`, `edition = "2024"`.
- `cargo fmt`, and `cargo clippy --all-targets -- -D warnings` must pass.

---

## 14. Acceptance criteria

These will be checked in review. Items marked 🔌 need the real treadmill and are checked by the
user; all others must pass inside a VM without Bluetooth hardware.

1. `nix build .#pitpat` and `nix build .#plasmoid` succeed; `nix flake check` passes, including
   the NixOS VM test.
2. `cargo test` passes and includes the golden vectors from §3.4, decoder tests (states, units,
   short packet), queue semantics (§3.5: coalescing, stop to the front, capacity), lifetime-delta
   logic (§4.4: reset, absurd delta, reconnect continuation), session rules (§4.3: pause doesn't
   split, noise discarded), target auto-stop.
3. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
4. `pitpat daemon --simulate --state-dir <tmp>`:
   - `pitpat start 3` → state goes `starting` → `running`, speed ramps to 3.0;
   - `pitpat speed +0.1` → target 3.1;
   - `/metrics` shows growing `pitpat_distance_meters_total`;
   - `pitpat target --distance 50m` → stops automatically, and a session with
     `end_reason: target_reached` appears in `pitpat sessions`;
   - after a daemon restart the lifetime totals are preserved.
5. Security: with default settings, a `POST` without `Content-Type: application/json` gets 415;
   a request with `Host: evil.example` gets 421; `--listen 0.0.0.0:9839` without
   `--token-file` refuses to start; with a token, a POST without it gets 401.
6. RSS of `pitpat daemon --simulate` after 5 minutes of running is < 15 MiB (from
   `/proc/<pid>/status` `VmRSS`); release binary < 8 MiB.
7. `web/index.html` < 15 KiB, contains no `http://` or `https://` URLs, and works against the
   simulator (start/pause/stop/speed/presets/target).
8. The plasmoid loads with `plasmoidviewer -a it.lapo.pitpat` (or at least passes
   `qmllint`/`qml` syntax checks if no display is available), and shows the simulator's data.
9. 🔌 Connects to the real treadmill (auto-discovery and pinned address), start/pause/stop/speed
   work, belt keeps running for > 10 minutes (heartbeat ok), metrics match the treadmill's display,
   reconnects after the treadmill is power-cycled, and SIGTERM while walking stops the belt.
10. 🔌 Widget in the Plasma panel: wheel and middle-click work, milestone notification appears.
11. Every source file has the SPDX header; `LICENSE`, `Cargo.toml` and `metadata.json` all say
    ISC and Lapo Luchini <lapo@lapo.it>.
