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
      # The module is exported as a function so its package defaults can
      # close over this flake's builds (REVIEW item 2): a bare module
      # would resolve `pkgs.callPackage ./package.nix { }` against the
      # user's nixpkgs without our `src`, which fails with pkgs.src.
      nixosModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.pitpat.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.pitpat;
          services.pitpat.widget.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.plasmoid;
        };
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

          # REVIEW item 2: a bare `services.pitpat.enable = true` must
          # evaluate without pinning the package explicitly.
          module-defaults = (nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              (
                { lib, ... }:
                {
                  services.pitpat.enable = true;
                  fileSystems."/".device = "nodev";
                  fileSystems."/".fsType = "none";
                  boot.loader.grub.enable = false;
                  system.stateVersion = "25.05";
                }
              )
            ];
          }).config.system.build.toplevel;
        }
      );
    };
}
