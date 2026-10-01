{
  description = "Perry-WIT: Hermetic TypeScript compiler and WASI Preview 2 component toolchain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    wasi = {
      url = "github:WebAssembly/WASI/v0.2.6";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, wasi }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # Dynamic WASI Preview 2 WIT definitions extracted from official WASI v0.2.6
        # Consolidate each package's WIT files into a deterministic package.wit
        wasiWit = pkgs.runCommand "wasi-preview2-wit" {} ''
          mkdir -p "$out"
          for pkg in cli clocks filesystem http io random sockets; do
            mkdir -p "$out/$pkg"
            pkg_header=$(grep -h "^package wasi:" "${wasi}/wasip2/$pkg"/*.wit | head -n 1)
            echo "$pkg_header" > "$out/$pkg/package.wit"
            for f in "${wasi}/wasip2/$pkg"/*.wit; do
              sed "/^package wasi:/d" "$f" >> "$out/$pkg/package.wit"
            done
          done
        '';

        # Common source filter for Rust crate builds
        commonFilter = path: type:
          (pkgs.lib.hasSuffix ".wit" path) ||
          (pkgs.lib.hasSuffix ".ts" path) ||
          (pkgs.lib.hasSuffix ".json" path) ||
          (pkgs.lib.hasSuffix ".toml" path) ||
          (pkgs.lib.hasSuffix ".lock" path) ||
          (craneLib.filterCargoSources path type);

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = commonFilter;
        };

        # Shared vendored dependencies artifacts
        cargoArtifacts = craneLib.buildDepsOnly {
          inherit src;
          pname = "perry-wit-deps";
          version = "0.1.0";
          strictDeps = true;
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config cacert ];
          preBuild = ''
            mkdir -p wit/deps
            ln -sfn "${wasiWit}"/* wit/deps/
          '';
        };

        # Precompiled guest runtime WebAssembly module (wasm32-unknown-unknown with imported memory)
        guestRuntime = craneLib.buildPackage {
          inherit src cargoArtifacts;
          pname = "guest-runtime";
          version = "0.1.0";
          cargoExtraArgs = "--package guest-runtime --target wasm32-unknown-unknown";
          RUSTFLAGS = "-C link-arg=--import-memory -C link-arg=--global-base=1048576 -C link-arg=--no-entry";
          preBuild = ''
            mkdir -p wit/deps
            ln -sfn "${wasiWit}"/* wit/deps/
          '';
          doCheck = false;
          installPhaseCommand = ''
            mkdir -p "$out/lib"
            cp target/wasm32-unknown-unknown/release/guest_runtime.wasm "$out/lib/"
          '';
        };

        # Perry-WIT CLI binary build (statically embeds guest_runtime.wasm)
        perryWitBin = craneLib.buildPackage {
          inherit src cargoArtifacts;
          pname = "perry-wit";
          version = "0.1.0";
          strictDeps = true;
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config ];
          GUEST_RUNTIME_PATH = "${guestRuntime}/lib/guest_runtime.wasm";
        };

        # Example WASIp2 component hermetically compiled using perry-wit CLI and dynamic WASI WIT
        exampleMergeDocs = pkgs.stdenv.mkDerivation {
          pname = "example-merge-docs";
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin ];
          buildPhase = ''
            export WASI_WIT_PATH="${wasiWit}"
            mkdir -p dist
            perry-wit examples/merge_docs.ts \
              --wit wit \
              --world merge-docs \
              -o dist/perry_merge_docs.stripped.wasm
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp dist/perry_merge_docs.stripped.wasm "$out/lib/"
          '';
        };

        # Example WASIp2 task component with Canonical ABI export trampolines
        exampleMergeTask = pkgs.stdenv.mkDerivation {
          pname = "example-merge-task";
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin pkgs.wasmtime ];
          buildPhase = ''
            export HOME="$TMPDIR"
            export WASMTIME_CACHE_ENABLED=false
            export WASI_WIT_PATH="${wasiWit}"
            mkdir -p dist
            perry-wit examples/merge_task.ts \
              --wit wit \
              --world task-runner \
              -o dist/perry_merge_task.wasm
            wasmtime run -C cache=n -S http=y -S inherit-network=y --invoke 'run-task("hermetic-build")' dist/perry_merge_task.wasm
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp dist/perry_merge_task.wasm "$out/lib/"
          '';
        };

      in {
        packages = {
          default = perryWitBin;
          perry-wit = perryWitBin;
          guest-runtime = guestRuntime;
          example-merge-docs = exampleMergeDocs;
          example-merge-task = exampleMergeTask;
          wasi-wit = wasiWit;
        };

        checks = {
          perry-wit-fmt = craneLib.cargoFmt {
            inherit src;
          };
          inherit exampleMergeDocs exampleMergeTask guestRuntime perryWitBin;
        };

        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
            pkgs.wasmtime
            pkgs.wasm-tools
            pkgs.nodejs
            pkgs.wkg
            pkgs.pkg-config
            pkgs.cacert
          ];

          shellHook = ''
            export WASI_WIT_PATH="${wasiWit}"
            export GUEST_RUNTIME_PATH="${guestRuntime}/lib/guest_runtime.wasm"
            if [ ! -e wit/deps ]; then
              mkdir -p wit
              ln -sfn "${wasiWit}" wit/deps
            fi
            echo "=== Perry-WIT Hermetic Environment ==="
            echo "wasmtime:   $(${pkgs.wasmtime}/bin/wasmtime --version)"
            echo "node:       $(${pkgs.nodejs}/bin/node --version)"
            echo "rustc:      $(${toolchain}/bin/rustc --version)"
            echo "WASI WIT:   $WASI_WIT_PATH"
            echo "RT Build:   $GUEST_RUNTIME_PATH"
            echo "======================================"
          '';
        };
      }
    );
}
