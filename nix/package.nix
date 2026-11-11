# SPDX-License-Identifier: ISC
{
  lib,
  rustPlatform,
  pkg-config,
  dbus,
  src,
}:

# `src` is the flake root passed from flake.nix (a store path); a relative
# `../..` would point *outside* the flake source when evaluated from the
# store copy, which is why pure eval rejects it.

rustPlatform.buildRustPackage {
  pname = "pitpat";
  version = "0.1.0";
  inherit src;

  cargoLock.lockFile = src + "/Cargo.lock";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ dbus ];

  # the unit-test suite: golden protocol vectors, queue semantics,
  # delta logic, sessions, targets
  doCheck = true;

  meta = with lib; {
    description = "PitPat walking-pad treadmill BLE controller and Prometheus exporter";
    longDescription = ''
      A daemon owning the single BLE connection to a PitPat walking-pad
      treadmill, exposing Prometheus metrics (most importantly the distance
      walked), a JSON control API, and a simulator mode; plus CLI subcommands
      for scripting and KDE global shortcuts.
    '';
    homepage = "https://github.com/lapo-luchini/pitpat-exporter";
    license = licenses.isc;
    mainProgram = "pitpat";
    platforms = platforms.linux;
  };
}
