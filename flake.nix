{
  # Reproducible Pi Zero 2W (aarch64) appliance build for bris.
  #
  # The Bris appliance is a single-binary-per-device frontend: the `bris` CLI
  # (crates/bris-cli) cross-compiled for aarch64-unknown-linux-gnu, bundled with
  # its runtime prerequisites (camera / V4L2 access via bris-capture, serial +
  # UDP NMEA sinks via bris-nmea, the bris-data ml-gravity payload) as one
  # appliance image.
  #
  # This flake pins ALL build inputs (nixpkgs rev + the aarch64 GNU cross
  # toolchain + the pinned Rust from rust-toolchain.toml), so `nix develop` /
  # `nix build` produce the same cross toolchain on any host — no host apt, no
  # sudo, no `gcc-aarch64-linux-gnu` package to install. It is the reproducible
  # substrate for `scripts/pi-appliance/build.sh`.
  #
  # NO infra identifiers (hostnames, IPs, device names, credentials) are baked
  # in here or anywhere in the recipe — the appliance is provisioned with its
  # site config at flash/first-boot time, never at build time.

  description = "bris Pi Zero 2W (aarch64) appliance cross-build";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      # Build host is x86_64 Linux (the workstation / CI runner). The appliance
      # target is aarch64 Linux (Pi Zero 2W class).
      buildSystem = "x86_64-linux";
      pkgs = import nixpkgs { system = buildSystem; };

      # aarch64 GNU cross toolchain whose binaries carry the target-triple
      # prefix `aarch64-unknown-linux-gnu-*`, matching the Rust target triple
      # and the linker/CC names configured in `.cargo/config.toml`.
      crossCC = pkgs.pkgsCross.aarch64-multiplatform.buildPackages.gcc;
    in
    {
      # Reproducible cross toolchain + build tools. Enter with `nix develop`,
      # then run `scripts/pi-appliance/build.sh`. The cross gcc lands on PATH as
      # `aarch64-unknown-linux-gnu-gcc`, which is exactly what
      # `.cargo/config.toml` names as the linker/CC for the appliance target.
      #
      # Rust itself is intentionally NOT provided here: the workspace pins its
      # toolchain via `rust-toolchain.toml` (1.94) and builds under the host's
      # rustup, matching CI. This shell only adds the cross toolchain that the
      # host lacks, so it never shadows the pinned cargo/rustc.
      devShells.${buildSystem}.default = pkgs.mkShell {
        packages = [
          crossCC
          pkgs.pkg-config
          # bris-cli transitively needs libclang for bindgen (v4l2-sys-mit via
          # bris-capture, and ort-sys) even though the target is aarch64 — the
          # bindgen invocation itself runs on the HOST (x86_64), it just parses
          # C headers to emit target-agnostic Rust bindings. Without a libclang
          # on PATH/LIBCLANG_PATH the build panics
          # "Unable to find libclang ... set LIBCLANG_PATH".
          pkgs.clang
          pkgs.llvmPackages.libclang
        ];

        # Make the cross compiler / linker discoverable to cargo + cc-rs without
        # relying on the host. These mirror `.cargo/config.toml`'s [env] block
        # (force=false there, so this shell export wins if it differs).
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = "aarch64-unknown-linux-gnu-gcc";
        CC_aarch64_unknown_linux_gnu = "aarch64-unknown-linux-gnu-gcc";
        CXX_aarch64_unknown_linux_gnu = "aarch64-unknown-linux-gnu-g++";
        AR_aarch64_unknown_linux_gnu = "aarch64-unknown-linux-gnu-ar";

        # bindgen (used transitively by v4l2-sys-mit / ort-sys) needs to find
        # libclang at build time; point it at the pinned nix libclang so the
        # shell is reproducible with no host libclang-dev required.
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

        shellHook = ''
          echo "bris Pi appliance cross-build shell (aarch64-unknown-linux-gnu)."
          echo "cross gcc: $(command -v aarch64-unknown-linux-gnu-gcc || echo MISSING)"
          echo "LIBCLANG_PATH: ''${LIBCLANG_PATH:-MISSING}"
          echo "run: scripts/pi-appliance/build.sh"
        '';
      };

      # Expose the pinned cross toolchain as a package so CI / other tooling can
      # `nix build .#crossToolchain` and put it on PATH deterministically.
      packages.${buildSystem}.crossToolchain = crossCC;
    };
}
