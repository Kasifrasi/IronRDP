{
  description = "Nix flake for IronRDP wgpu client development";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        lib = pkgs.lib;

        just-wrapper = pkgs.writeShellApplication {
          name = "just";
          runtimeInputs = [ pkgs.just pkgs.git ];
          text = ''
            # Finde das Root-Verzeichnis des IronRDP-Projekts
            PROJECT_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
            
            # Leite just an den WGPU-Client weiter. 
            # '$@' übergibt alle deine Argumente (wie 'dev' oder 'test') nahtlos.
            exec just --justfile "$PROJECT_ROOT/crates/ironrdp-client-wgpu/Justfile" "$@"
          '';
        };

        # Rust toolchain: version pinned by ../../rust-toolchain.toml,
        # plus extra extensions for IDE support
        rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ../../rust-toolchain.toml).override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        # Upper-case host triple for CARGO_TARGET_* env vars (works on any arch)
        targetUpper = builtins.replaceStrings
          [ "a" "b" "c" "d" "e" "f" "g" "h" "i" "j" "k" "l" "m"
            "n" "o" "p" "q" "r" "s" "t" "u" "v" "w" "x" "y" "z" "-" ]
          [ "A" "B" "C" "D" "E" "F" "G" "H" "I" "J" "K" "L" "M"
            "N" "O" "P" "Q" "R" "S" "T" "U" "V" "W" "X" "Y" "Z" "_" ]
          pkgs.stdenv.hostPlatform.config;

        # Libraries required at runtime by winit (X11/Wayland) and wgpu (GPU)
        runtimeLibs = with pkgs; [
          # X11 (winit)
          libX11

          # Wayland (winit)
          wayland
          libxkbcommon
          libdecor

          # GPU (wgpu)
          libGL
          vulkan-headers
          vulkan-loader

          # Misc
          fontconfig
          dbus
        ];

        # Build-time tools and dependencies
        nativeTools = with pkgs; [
          pkg-config
          mold
          clang
          cmake

          # Rust tools
          bacon
          cargo-release
          cargo-about
          cargo-audit
          cargo-cyclonedx
          cargo-deny
          cargo-edit
          cargo-expand
          cargo-license
          cargo-llvm-cov
          cargo-nextest
          sccache
          just-wrapper
        ] ++ [ rustToolchain ];

      in {
        devShells.default = pkgs.mkShellNoCC {
          buildInputs = runtimeLibs;

          nativeBuildInputs = nativeTools;

          # Environment variables — arch-agnostic via dynamic target triple
          CC = "clang";
          CXX = "clang++";
          RUSTC_WRAPPER = "sccache";

          # Ensure runtime libraries can be found by dynamically loaded libraries
          LD_LIBRARY_PATH = "${lib.makeLibraryPath runtimeLibs}:/run/opengl-driver/lib:/run/opengl-driver-32/lib";

          shellHook = ''
            export CARGO_TARGET_${targetUpper}_LINKER="clang"
            export CARGO_TARGET_${targetUpper}_RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            echo '🦀 IronRDP Dev Environment'
            echo 'Run `just --list` for available tasks.'
          '';
        };
      });
}
