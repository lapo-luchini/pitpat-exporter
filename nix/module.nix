# SPDX-License-Identifier: ISC
{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.pitpat;

  flags = concatStringsSep " " (
    [ ]
    ++ optional (cfg.address != null) "--address ${escapeShellArg cfg.address}"
    ++ optional (cfg.adapter != null) "--adapter ${escapeShellArg cfg.adapter}"
    ++ optional (cfg.weightKg != null) "--weight-kg ${toString cfg.weightKg}"
    ++ optional (cfg.presets != [ ]) "--presets ${
      concatMapStringsSep "," toString cfg.presets
    }"
    ++ optional cfg.simulate "--simulate"
    ++ [ "--listen ${escapeShellArg cfg.listen}" ]
    ++ optionals (cfg.tokenFile != null) [
      # carried by LoadCredential into the service's credential directory
      "--token-file %d/pitpat.token"
    ]
    ++ cfg.extraArgs
  );

in

{
  # ------------------------------------------------------------------ opts
  options.services.pitpat = {
    enable = mkEnableOption "pitpat treadmill control daemon and Prometheus exporter";

    package = mkOption {
      type = types.package;
      # default comes from the flake wrapper (flake.nix closes over `self`)
      defaultText = "the pitpat flake package";
      description = "The pitpat package.";
    };

    address = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Treadmill MAC address; null = auto-discovery by the fba0 service UUID.";
    };

    adapter = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        Bluetooth adapter: a name such as "hci0", or the controller's MAC address
        (see `bluetoothctl list`). Prefer the MAC: hciN names can change when a
        controller resets. null = BlueZ default adapter.
      '';
    };

    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:9839";
      description = "HTTP listen address; keep on loopback unless you use a token file.";
    };

    tokenFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Shared secret file; required when listen is not loopback (asserted below).";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Opens the HTTP port in the firewall (only sensible with tokenFile).";
    };

    weightKg = mkOption {
      type = types.nullOr (types.ints.u8);
      default = null;
      description = "User weight in kg, forwarded to the treadmill (used for calories).";
    };

    presets = mkOption {
      type = types.listOf (types.either types.int types.float);
      default = [
        2
        3
        4
      ];
      description = "Speed presets (km/h) shown in GUIs.";
    };

    simulate = mkOption {
      type = types.bool;
      default = false;
      description = "Run against the built-in simulator instead of real Bluetooth.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Extra `pitpat daemon` flags, e.g. [ \"--log\" \"debug\" ].";
    };

    widget = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Install the Plasma 6 plasmoid alongside the CLI.";
      };
      package = mkOption {
        type = types.package;
        defaultText = "the plasmoid from this flake";
        description = "The plasmoid package to install.";
      };
    };
  };

  # ---------------------------------------------------------------- options
  config = mkIf cfg.enable {
    hardware.bluetooth.enable = true;


    # a non-loopback bind without a token would allow anyone reachable to
    # trigger the treadmill — refuse at evaluation time (§6.1)
    assertions = [
      {
        assertion =
          cfg.listen != null
          && (
            builtins.match ''127\.0\.0\.1:.*'' cfg.listen != null
            # POSIX ERE has no \] outside a class: use []] for a literal ]
            || builtins.match ''\[::1[]]:.*'' cfg.listen != null
            || builtins.match ''localhost:[0-9]+'' cfg.listen != null
            || cfg.listen == "localhost"
            || cfg.tokenFile != null
          );
        # keep the loopback notion identical to the daemon's
        # (src/config.rs is_loopback_listen, REVIEW item 5)
        message = "services.pitpat: a non-loopback listen requires services.pitpat.tokenFile";
      }
    ];

    networking.firewall.allowedTCPPorts = mkIf (cfg.openFirewall && cfg.listen != null) [
      (toInt (last (splitString ":" cfg.listen)))
    ];

    systemd.services.pitpat = {
      description = "pitpat treadmill control daemon";
      wantedBy = [ "multi-user.target" ];
      after = [
        "bluetooth.service"
        "network.target"
      ];
      wants = [ "bluetooth.service" ];

      serviceConfig = {
        Restart = "on-failure";
        RestartSec = 5;
        DynamicUser = "yes";
        StateDirectory = "pitpat";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        CapabilityBoundingSet = [ ];
        MemoryMax = "64M";
        ExecStart = [
          "${cfg.package}/bin/pitpat daemon ${
            strings.concatStringsSep " " [
              "--state-dir /var/lib/pitpat"
              flags
            ]
          }"
        ];
        LoadCredential = optionals (cfg.tokenFile != null) [
          "pitpat.token:${cfg.tokenFile}"
        ];

        # NOTE on the DynamicUser + org.bluez question (§11.1): a DynamicUser
        # with a plain — but hard-restrictive — D-Bus access still usually
        # works because BlueZ's D-Bus policy (org.bluez.conf) allows any
        # caller on the SYSTEM bus by default. If a hardened environment
        # ('--address') denies connects at the dbus daemon level, switch to
        # a static user/group pair plus a services.dbus.packages policy
        # snippet instead:
        #
        #   users.users.pitpat = { isSystemUser = true; group = "pitpat"; };
        #   services.dbus.packages = [ pitpatDbusPolicy ];  # <allow user="pitpat"/>
        #
        # and remove DynamicUser/StateDirectory in favour of the static user.
      };
    };

    # pitpat on PATH so the CLI can serve global shortcuts (§8.1)
    environment.systemPackages =
      [ cfg.package ]
      ++ optional cfg.widget.enable cfg.widget.package;
  };
}
