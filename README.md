# pitpat-exporter

Control a PitPat walking-pad treadmill over Bluetooth LE from Linux (NixOS / KDE Plasma 6),
and export its data as Prometheus metrics — most importantly **the distance walked**,
both for the current session and as a lifetime counter.

Protocol reverse-engineering credit: the [PitPat-WebBT](https://github.com/KeiranY/PitPat-WebBT)
project (`treadmill.js`); everything needed is documented in [`SPEC.md`](SPEC.md) §3.

## Components

- `pitpat daemon` — single process owning the BLE connection, reconnecting forever, with an
  HTTP/JSON control API and Prometheus metrics on `127.0.0.1:9839` (see `--simulate` for
  hardware-free development/testing).
- Plasma 6 widget (`it.lapo.pitpat`) for the panel with popup controls.
- `pitpat` CLI for scripting and KDE global shortcuts (see below).

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

## Safety note about remote control

The API can make the belt start moving on a machine someone may be standing on. Keep the
default loopback bind, use `--token-file` whenever the port is exposed, and check the very
first start on real hardware at a low speed.
