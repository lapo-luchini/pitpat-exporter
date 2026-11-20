# pitpat-exporter

Control a PitPat walking-pad treadmill over Bluetooth LE from Linux (NixOS / KDE Plasma 6),
and export its data as Prometheus metrics — most importantly **the distance walked**,
both for the current session and as a lifetime counter.

Protocol reverse-engineering credit: the [PitPat-WebBT](https://github.com/KeiranY/PitPat-WebBT)
project (`treadmill.js`). Writes must be ATT **Write Requests** (`writeValue` with response) —
the treadmill silently ignores Write Commands.

## Components

- `pitpat daemon` — single process owning the BLE connection on demand, with an HTTP/JSON
  control API and Prometheus metrics on `127.0.0.1:9839` (see `--simulate` for hardware-free
  development/testing).
- Plasma 6 widget (`it.lapo.pitpat`) for the panel with popup controls.
- `pitpat` CLI for scripting and KDE global shortcuts (see below).

## States

The API, `--json` output, metrics and `sessions.jsonl` use the machine
ids (`running`, `starting`, `paused`, `stopped`, `sleeping`,
`disconnected`). Human-facing text (web page, widget, CLI without
`--json`) says **walking** for `running`, `starting…`, `paused`,
`stopped`, **asleep** for `sleeping`, and calls a session a **walk**.

## Idle sleep (on-demand connection)

The treadmill beeps on every BLE connect and keeps its LCD lit, so the daemon does not hold the
link permanently. After `--idle-timeout` seconds (default 300; `0` = always connected) stopped
with no control call it disconnects and sleeps: no reconnects, no beeps. Any control call wakes
it — `pitpat start/pause/stop/toggle` or an explicit `pitpat connect` / `POST /api/connect`.
Speed changes (absolute, delta, preset) while asleep only update the remembered target and do
not wake it (a stray mouse-wheel won't beep). `pitpat sleep` / `POST /api/sleep` drops the link
early.

Notes:

- Writes to the treadmill need BlueZ's experimental `ConnectDevice` so a wake never has to scan
  (some USB dongles crash under overlapping scans). The NixOS module sets
  `hardware.bluetooth.settings.General.Experimental = true` by default; on other setups add:

  ```ini
  [General]
  Experimental = true
  ```

- A walk started **and** finished with the physical remote while the daemon sleeps is not
  recorded; a walk in progress when the daemon wakes is credited once (capped at 20 km /
  30 000 steps).

## Quick start (simulator)

```console
$ pitpat daemon --simulate
$ pitpat start 3
$ pitpat status
$ curl -s http://127.0.0.1:9839/metrics | grep distance
$ pitpat target --distance 50m
```

## NixOS

Add the flake as input and enable the module:

```nix
{
  inputs.pitpat.url = "git+ssh://…/pitpat_exporter";

  outputs = { self, nixpkgs, pitpat, ... }: let system = "x86_64-linux"; in {
    nixosConfigurations.mybox = nixpkgs.lib.nixosSystem {
      system = system;
      modules = [
        pitpat.nixosModules.default
        {
          services.pitpat.enable = true;
          # services.pitpat.widget.enable = true;  # Plasma 6 plasmoid + CLI
        }
      ];
    };
  };
}
```

The service is hardened (DynamicUser, ProtectSystem=strict, …) and talks to `org.bluez`.
Metrics are scraped like any HTTP endpoint:
`services.prometheus.scrapeConfigs = [{ job_name = "pitpat"; static_configs = [{ targets = [ "127.0.0.1:9839" ]; }]; }];`
A ready-made Grafana dashboard ships in `contrib/grafana/pitpat.json`.

Prometheus/Grafana snippet (any Linux, journald logs):

```ini
[Service]
ExecStart=/usr/local/bin/pitpat daemon
```

## Non-Nix build

```console
$ cargo build --release
$ target/release/pitpat daemon &
```

## CLI and shortcuts

```
pitpat status [--json]
pitpat start [SPEED_KMH]
pitpat pause
pitpat toggle          # running → pause; otherwise start
pitpat stop
pitpat speed 3.2 | +0.1 | -0.1 | preset 2
pitpat target [--distance 3km|2500m] [--time 30m|1h15m] | pitpat target clear
pitpat connect              # wake the daemon (bring the BLE link up)
pitpat sleep                # drop the BLE link now
pitpat sessions [-n 10]
```

Client flags: `--url` (default `http://127.0.0.1:9839`, env `PITPAT_URL`) and
`--token-file` (env `PITPAT_TOKEN_FILE`).

### Global shortcuts (Plasma)

A plasmoid can't register several global shortcuts, so bind the CLI in
*System Settings → Keyboard → Shortcuts → Add New → Command*:

| Shortcut | Command |
|---|---|
| Meta+Space | `pitpat toggle` |
| Meta+Up | `pitpat speed +0.1` |
| Meta+Down | `pitpat speed -0.1` |
| Meta+End | `pitpat stop` |

## Troubleshooting

Walks not saved: the state dir may contain files left by a manual run
as another user (`chown` the files in `/var/lib/pitpat` to match the
directory). The daemon keeps the last walks in memory and saves them as
soon as the files become writable; the web page and the widget show a
warning while saving fails (`persist_ok: false` in `/api/state`,
`pitpat_persist_errors_total`).

## Releasing

`Cargo.toml` carries the last release's version; `pitpat --version`,
`/metrics` `build_info` and the widget's About page show it plus the git
describe suffix (e.g. `1.3.0-3-gabc1234`; nix builds show the exact
commit as `1.3.0-gabc1234` since the store copy has no `.git`).

To release a new version:

1. once per clone, enable the pre-push hook:
   `git config core.hooksPath contrib/githooks`;
2. `contrib/release.sh X.Y.Z` — it refuses anything but a clean tree on
   `trunk` and a version strictly newer than the current one, bumps
   `Cargo.toml`, `Cargo.lock` and the plasmoid's `metadata.json`, runs
   `cargo test`, commits `release X.Y.Z` and tags `X.Y.Z`;
3. check the commit and the tag, then `git push --follow-tags`.

Two guards keep the tags and the version in step: `cargo test`'s
`version_matches_git_tag` fails when something was tagged without the
bump (or HEAD sits on a tag that disagrees with `Cargo.toml`), and the
pre-push hook refuses any pushed tag whose name differs from the
`Cargo.toml` version of the tagged commit.

## Safety note about remote control

The API can make the belt start moving on a machine someone may be standing on. Keep the
default loopback bind, use `--token-file` whenever the port is exposed, and check the very
first start on real hardware at a low speed.
