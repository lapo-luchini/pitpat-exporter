# Hardware captures

Real logs from PitPat treadmills, kept so that the protocol decoder and the session logic can be
tested without the hardware. Each device has its own folder; add one per model or firmware rather
than mixing them.

| Folder | Device | Status frames |
|---|---|---|
| `device-a/` | The maintainer's PitPat (BLE name and serial in the `68 34` frame) | `68 34` (52 bytes) on connect, then `68 32` (50 bytes) |

## File format

Plain text, as it came out of the daemon or `journalctl`. A test should look only at what it needs:

- `bytes=68 …` lines are status notifications. Older logs say `status notification bytes=`,
  newer ones `notify bytes=` or `undecodable status notification bytes=`.
- `write bytes=6a …` lines are commands the daemon sent. `6a 05 fd f8 43` is the keep-alive poll;
  `6a 17 …` (23 bytes) is a control or SetSpeed command.
- Lines starting with `#` at the top are notes added afterwards: what the person did, and when.
- Some files hold frames already **decoded** by [`frames.awk`](frames.awk)
  (`HH:MM:SS state=.. speed=.. dist=.. dur=.. kcal@18=.. steps@43=.. raw41=..`), because only that
  output was saved. They are still good ground truth for sessions and steps, but not for byte-level
  decoder tests.

## device-a

| File | What it shows |
|---|---|
| `2026-11-13-first-connect-68-34.log` | Daemon run with `--log debug`: connect, start from the remote, speed changes, stop. Every frame is `68 34`. The commands sent then were not yet accepted by the belt. |
| `2026-11-16-paused-68-32.log` | One `68 34` connect frame, then raw `68 32` frames while paused, plus writes. |
| `2026-11-17-walk-60-steps.log` | Decoded frames and raw writes for a walk: SetSpeed while paused (starts at 1.0 and loses the speed), 15 s off the belt (no auto-pause), pause and resume (counters kept), 60 hand-counted steps from 12:08:40 to 12:09:20, stop (steps stay at 268 afterwards). |
| `2026-11-17-step-styles.log` | Decoded frames: hand-counted normal (86), long (65) and baby (114) steps, one minute each, then 104 normal steps at 4 km/h. The belt's count matches normal and long steps and under-counts baby steps (77). Stop resets steps to 0 here. |

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
