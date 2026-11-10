# NixOS VM test (§11): the daemon runs against the built-in simulator;
# /healthz answers, /metrics contains the lifetime counter, and
# POST /api/start takes the state to `running`.
{
  lib,
  pkgs,
  pitpatPackage,
  nixpkgsPath,
  config ? { },
  ...
}:

{
  name = "pitpat-simulate";

  # run QEMU without hardware KVM (works on any builder; slower)
  requiredFeatures.kvm = false;

  nodes.machine =
    { ... }:
    {
      imports = [
        ./module.nix
        # no-KVM VM: keep the boot lean so software emulation finishes
        "${nixpkgsPath}/nixos/modules/profiles/minimal.nix"
      ];
      environment.systemPackages = [ pkgs.curl ];
      virtualisation.memorySize = 1024;
      virtualisation.cores = 2;
      # no external networking needed for the test; skipping DHCP shaves
      # boot seconds on the TCG (software emulation) builder
      networking.useDHCP = false;
      services.pitpat = {
        enable = true;
        package = pitpatPackage;
        simulate = true;
      };

      # keep the QEMU VM lean
      virtualisation.graphics = false;
    };

  testScript = ''
    machine.wait_for_unit("pitpat.service")
    machine.wait_for_open_port(9839)

    # daemon alive regardless of BLE state
    machine.succeed("curl -sf http://127.0.0.1:9839/healthz | grep -q '^ok$'")

    # Prometheus text exposes the lifetime counter
    machine.succeed(
      "curl -sf http://127.0.0.1:9839/metrics | grep -q pitpat_distance_meters_total"
    )

    # starting the belt: state goes starting → running within the simulator
    machine.succeed(
      "curl -sf -X POST -H 'Content-Type: application/json' "
      "-d '{}' http://127.0.0.1:9839/api/start"
    )
    with subtest("state reaches running"):
        machine.wait_until_succeeds(
            "curl -sf http://127.0.0.1:9839/api/state | grep -q '\"state\":\"running\"'",
            timeout=30,
        )
        machine.succeed(
            "curl -sf http://127.0.0.1:9839/metrics | grep -q 'pitpat_session_distance_meters '"
        )
  '';
}
