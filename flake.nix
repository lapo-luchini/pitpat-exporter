{
  description = "pitpat: BLE control + Prometheus export for PitPat walking-pads (NixOS module included)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs [
          "x86_64-linux"
          "aarch64-linux"
        ] (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
          }
        );
    in
    {
      nixosModules.default = ./nix/module.nix;
      nixosModules.pitpat = self.nixosModules.default;

      packages = forAllSystems (
        { pkgs, ... }:
        {
          pitpat = pkgs.callPackage ./nix/package.nix { src = self; };
          plasmoid = pkgs.callPackage ./nix/plasmoid.nix { };
          default = self.packages.${pkgs.system}.pitpat;
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
              pkg-config
              dbus
            ];
          };
        }
      );

      checks = forAllSystems (
        { system, pkgs }:
        let
          # Host pkgs with a test-driver patch: under software emulation
          # (no KVM on every builder) the guest clock runs slower than the
          # host's, and the driver's hardcoded 5-min boot window expires
          # before systemd finishes. Give it 30 min (60 × 30 s).
          hostPkgs = pkgs.extend (
            final: prev: {
              nixos-test-driver = prev.nixos-test-driver.overrideAttrs (
                old: {
                  postInstall =
                    (old.postInstall or "")
                    + ''
                      f=$(find $out/lib -path "*test_driver/machine/__init__.py")
                      sed -i "s/for _ in range(10):/for _ in range(60):/" "$f"
                    '';
                }
              );
            }
          );
          nixosLib = import (nixpkgs + "/nixos/lib/testing-python.nix") {
            inherit system;
            pkgs = hostPkgs;
          };
        in
        {
          # the package build also runs `cargo test` (doCheck)
          pitpat = self.packages.${system}.pitpat;
          plasmoid = self.packages.${system}.plasmoid;

          # VM test: service with --simulate, /healthz, /metrics, /api/start
          pitpat-simulate = nixosLib.runTest {
            imports = [ ./nix/vm-test.nix ];
            _module.args = {
              pitpatPackage = self.packages.${system}.pitpat;
              nixpkgsPath = nixpkgs;
            };
          };
        }
      );
    };
}
