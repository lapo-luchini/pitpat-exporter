# Hardware captures

Real logs from PitPat treadmills, kept so that the protocol decoder and the session logic can be
tested without the hardware. Each device has its own folder; add one per model or firmware rather
than mixing them.

| Folder | Device | Status frames |
|---|---|---|
| `device-a/` | The maintainer's PitPat (BLE name and serial in the `68 34` frame) | `68 34` (52 bytes) on connect, then `68 32` (50 bytes) |
| `device-b/` | PitPat-T01, SupeRun-built (service `ffff`, §3.7) | `4d 04 <seq> 2f` frames (51 bytes) around a 47-byte `68 2f` packet |

## File format

Plain text, as it came out of the daemon or `journalctl`. A test should look only at what it needs:

- `bytes=68 …` lines are status notifications. Older logs say `status notification bytes=`,
  newer ones `notify bytes=` or `undecodable status notification bytes=`.
- `write bytes=6a …` lines are commands the daemon sent. `6a 05 fd f8 43` is the keep-alive poll;
  `6a 17 …` (23 bytes) is a control or SetSpeed command.
- Lines starting with `#` at the top are notes added afterwards: what the person did, and when.
- [`frames.awk`](frames.awk) prints one decoded line per second from any of these files
  (`awk -f frames.awk FILE`), which is the quickest way to read a capture by eye.

## device-a

| File | What it shows |
|---|---|
| `2026-11-13-first-connect-68-34.log` | Daemon run with `--log debug`: connect, start from the remote, speed changes, stop. Every frame is `68 34`. The commands sent then were not yet accepted by the belt. |
| `2026-11-16-paused-68-32.log` | One `68 34` connect frame, then raw `68 32` frames while paused, plus writes. |
| `2026-11-17-walk-60-steps.log` | Full raw log of a walk: SetSpeed while paused (starts at 1.0 and loses the speed), 15 s off the belt (no auto-pause), pause and resume (counters kept), 60 hand-counted steps from 12:08:40 to 12:09:20, stop (steps stay at 268 afterwards). The daemon of that day recorded the session with `steps=0` (see the INFO lines in the header): a regression check for the steps fix. |
| `2026-11-17-step-styles.log` | Full raw log: hand-counted normal (86), long (65) and baby (114) steps, one minute each, then 104 normal steps at 4 km/h. The belt's count matches normal and long steps and under-counts baby steps (77). The count is 446 at stop and resets to 0 about 6 s later. |

## device-b

| File | What it shows |
|---|---|
| `2026-11-18-walk-20-steps.log` | Not a daemon log: three back-to-back recordings from the PitPat Walk web app (Web Bluetooth), converted line by line to this format (see its header). The belt runs at status speed `08 fc` the whole time and the page only writes the framed keep-alive every 200 ms: no start, speed change or stop. 20 hand-counted steps in the third recording, where the belt's count goes 213 → 232 and distance 100 → 110 m. No BLE address (Web Bluetooth hides it). `frames.awk` reads `68 32` frames only, so it prints nothing for this file. |

## Adding a capture from another treadmill

1. Run the daemon with debug logging for a whole walk, so that every frame and write is kept:
   `pitpat daemon --log debug 2>&1 | tee walk.log` (or, under systemd,
   `journalctl -u pitpat --since "…" --until "…" > walk.log`). Frames the decoder does not
   understand are still logged, as `undecodable status notification bytes=…`.
2. While walking, write down the wall-clock time of each action (connect, start, speed change,
   step off, pause, resume, stop) and, if you can, count steps for a minute. Put those notes as `#`
   lines at the top of the file.
3. Save the raw log, not decoded output, in a new folder such as `device-b/`, and add a row to the
   tables above with the model name from the box or app.

If the daemon cannot connect at all (different BLE service), a list of the device's services and
characteristics from nRF Connect or `bluetoothctl` is the most useful first capture.

Captures contain the BLE address and the serial number in the `68 34` frame. Only add a capture if
its owner is fine with those being public.
