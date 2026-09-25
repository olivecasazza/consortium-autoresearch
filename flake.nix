{
  description = "consortium — Rust rewrite of ClusterShell with Python bindings";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    git-hooks-nix.url = "github:cachix/git-hooks.nix";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      nixpkgs,
      flake-parts,
      crane,
      rust-overlay,
      git-hooks-nix,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ git-hooks-nix.flakeModule ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      # Non-per-system outputs
      flake = {
        # Nix library for fleet configuration (system-independent)
        lib = import ./nix/lib {
          inherit (nixpkgs) lib;
          writeText =
            name: text:
            builtins.toFile name text;
        };
      };

      perSystem =
        {
          config,
          system,
          pkgs,
          lib,
          ...
        }:
        let
          # ── Rust toolchain ─────────────────────────────────────────────
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };

          # Pinned in rust-toolchain.toml (read by rustup locally, mirrored
          # here so `nix build` uses the same channel) — avoids re-resolving
          # `stable.latest` on every rust-overlay/nixpkgs lock bump, which
          # rotated the toolchain and forced cold rebuilds.
          rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
            ];
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # ── Source filtering ───────────────────────────────────────────
          src = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || (builtins.match ".*\\.py$" path != null);
          };

          # ── Common Cargo args ──────────────────────────────────────────
          commonArgs = {
            inherit src;
            strictDeps = true;
            # PyO3 needs a Python interpreter at build time
            nativeBuildInputs = [ python ];
            # Tell pyo3-build-config where Python is
            PYO3_PYTHON = "${python}/bin/python3";
          };

          # ── Build artifacts (deps only, for caching) ───────────────────
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # ── The Rust library crate ─────────────────────────────────────
          consortium = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "-p consortium";
            }
          );

          # ── CLI binaries (claw, molt, pinch) ─────────────────────────
          consortium-cli = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "-p consortium-cli";
            }
          );

          # ── NixOS deployment library ────────────────────────────────
          consortium-nix = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "-p consortium-nix";
            }
          );

          # ── Nix library for fleet configuration ────────────────────
          consortiumLib = import ./nix/lib {
            inherit lib;
            inherit (pkgs) writeText;
          };

          # ── Python environment ─────────────────────────────────────────
          python = pkgs.python312;
          pythonEnv = python.withPackages (
            ps: with ps; [
              pytest
              pytest-timeout
              pytest-xdist
              pyyaml
            ]
          );

        in
        {
          # ── Pre-commit hooks ─────────────────────────────────────────
          # PR checks run on buildbot-nix, whose workers are x86_64-linux;
          # GitHub Actions runs no Nix. Other systems expose no checks.
          pre-commit.check.enable = system == "x86_64-linux";

          pre-commit.settings.hooks = {
            rustfmt = {
              enable = true;
              packageOverrides.cargo = rustToolchain;
              packageOverrides.rustfmt = rustToolchain;
            };
            # clippy runs via craneLib.cargoClippy in checks (with vendored deps);
            # the pre-commit hook can't vendor deps in --offline sandbox mode.
          };

          # ── Checks ─────────────────────────────────────────────────────
          # Built by buildbot-nix on every PR (repo topic `nixlab-ci`).
          checks = lib.optionalAttrs (system == "x86_64-linux") {
            # Rust unit tests
            cargo-test = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );

            # Clippy lints
            cargo-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- -D warnings";
              }
            );

            # Format check
            cargo-fmt = craneLib.cargoFmt {
              inherit src;
            };

            # Build the library
            inherit consortium;

            # The installed CLI starts (was the "Nix Integration" GitHub
            # Actions job).
            consortium-cli-smoke = pkgs.runCommand "consortium-cli-smoke" { } ''
              ${consortium-cli}/bin/cast --help >/dev/null
              ${consortium-cli}/bin/claw --help >/dev/null
              touch $out
            '';
          };

          # ── Packages ───────────────────────────────────────────────────
          packages = {
            inherit consortium consortium-cli consortium-nix;
            default = consortium;
          };

          # ── Dev shell ──────────────────────────────────────────────────
          devShells.default = pkgs.mkShell {
            inputsFrom = [ consortium ];

            nativeBuildInputs = [
              rustToolchain
              pythonEnv

              # Dev tools
              pkgs.cargo-watch
              pkgs.cargo-nextest
              pkgs.maturin

              # Nix tools
              pkgs.statix
              pkgs.deadnix
            ];

            shellHook = ''
              export RUST_BACKTRACE=1

              # Create a venv if one doesn't exist (maturin develop needs it)
              if [ ! -d .venv ]; then
                echo "  Creating .venv for maturin..."
                python3 -m venv .venv --system-site-packages
              fi
              source .venv/bin/activate

              export PYTHONPATH="$PWD/lib:$PYTHONPATH"
              ${config.pre-commit.installationScript}

              echo ""
              echo "  consortium dev shell"
              echo "  ─────────────────────────────────────────"
              echo "  cargo test                    — Rust tests"
              echo "  cargo watch -x test           — Rust TDD"
              echo "  maturin develop               — build Rust→Python bindings"
              echo "  pytest tests/ -v              — Python tests (original ClusterShell)"
              echo "  pytest tests/ -v              — Python tests (Rust-backed)"
              echo ""
            '';
          };
        };
    };
}
