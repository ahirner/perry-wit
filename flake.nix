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
    wasi-p3 = {
      url = "github:WebAssembly/WASI/v0.3.0";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, wasi, wasi-p3 }:
    let
      systemOutputs = flake-utils.lib.eachDefaultSystem (system:
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

        # The pinned 0.3 release keeps WIT under proposals, with native streams
        # replacing the separate P2 io package.
        wasiP3Wit = pkgs.runCommand "wasi-preview3-wit" {} ''
          mkdir -p "$out"
          for pkg in cli clocks filesystem http random sockets; do
            mkdir -p "$out/$pkg"
            cp "${wasi-p3}/proposals/$pkg/wit/"*.wit "$out/$pkg/"
          done
        '';

        checkP3Wit = pkgs.runCommand "check-wasi-preview3-wit" {
          nativeBuildInputs = [ pkgs.wasm-tools ];
        } ''
          mkdir -p probe/deps
          ln -s ${wasiP3Wit}/* probe/deps/
          cat > probe/world.wit <<'WIT'
          package perry:p3-check;
          world probe {
            include wasi:cli/imports@0.3.0;
            import wasi:http/types@0.3.0;
          }
          WIT
          wasm-tools component wit probe -o "$out"
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

        # Consumer helper to build components declaratively
        buildComponent = {
          name ? "component",
          src,
          entry ? "src/index.ts",
          wit ? "wit",
          world ? null,
        }: pkgs.stdenv.mkDerivation {
          pname = name;
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin pkgs.wasm-tools ];
          buildPhase = ''
            export HOME="$TMPDIR"
            export WASI_WIT_PATH="${wasiWit}"
            mkdir -p dist
            perry-wit ${entry} \
              --wit "${wit}" \
              ${if world != null then "--world " + world else ""} \
              -o "dist/${name}.wasm"
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp "dist/${name}.wasm" "$out/lib/"
          '';
        };

        # Consumer starter template component compiled with buildComponent
        templateComponent = buildComponent {
          name = "template-task";
          src = ./template;
          entry = "src/index.ts";
          wit = "wit";
          world = "task";
        };

        # Check: Verify template component builds and can be invoked directly
        checkTemplate = pkgs.runCommand "check-template-component" {
          nativeBuildInputs = [ pkgs.wasmtime ];
        } ''
          export HOME="$TMPDIR"
          export WASMTIME_CACHE_ENABLED=false
          wasmtime run -C cache=n -S http=y -S inherit-network=y --invoke 'run-task("template-hello")' "${templateComponent}/lib/template-task.wasm"
          touch "$out"
        '';

        # Check: Pre-commit / CI verification that generated TypeScript SDK definitions are valid and up to date
        sdkSyncCheck = pkgs.runCommand "check-sdk-sync" {
          nativeBuildInputs = [ perryWitBin pkgs.typescript ];
        } ''
          export HOME="$TMPDIR"
          export WASI_WIT_PATH="${wasiWit}"
          mkdir -p work/src work/wit work/.perry/types
          cp -r ${./wit}/* work/wit/
          cp ${./examples/merge_task.ts} work/src/merge_task.ts
          cd work
          perry-wit gen-types --wit wit --world merge-task --entry src/merge_task.ts
          tsc --noEmit
          touch "$out"
        '';

      in {
        packages = {
          default = perryWitBin;
          perry-wit = perryWitBin;
          guest-runtime = guestRuntime;
          example-merge-docs = exampleMergeDocs;
          example-merge-task = exampleMergeTask;
          template-component = templateComponent;
          wasi-wit = wasiWit;
          wasi-p3-wit = wasiP3Wit;
        };

        lib = {
          inherit buildComponent;
        };

        checks = {
          wasi-p3-wit = checkP3Wit;
          perry-wit-fmt = craneLib.cargoFmt {
            inherit src;
          };
          inherit exampleMergeDocs exampleMergeTask guestRuntime perryWitBin templateComponent checkTemplate sdkSyncCheck;
        };

        devShells.default = pkgs.mkShell {
          name = "perry-wit-dev";
          packages = [
            toolchain
            pkgs.wasmtime
            pkgs.wasm-tools
            pkgs.nodejs
            pkgs.typescript
            pkgs.wkg
            pkgs.pkg-config
            pkgs.cacert
          ];

          WASI_WIT_PATH = wasiWit;
          WASI_P3_WIT_PATH = wasiP3Wit;
          GUEST_RUNTIME_PATH = "${guestRuntime}/lib/guest_runtime.wasm";

          shellHook = ''
            if [ -d wit ] && [ ! -e wit/deps ]; then
              ln -sfn "${wasiWit}" wit/deps
            fi
            echo "Perry-WIT compiler development: cargo build, cargo test"
          '';
        };

        devShells.sdk = pkgs.mkShell {
          name = "perry-wit-sdk";
          packages = [
            perryWitBin
            pkgs.typescript
            pkgs.wasmtime
            pkgs.wasm-tools
          ];

          WASI_WIT_PATH = wasiWit;
          WASI_P3_WIT_PATH = wasiP3Wit;

          shellHook = ''
            if [ -d wit ]; then
              ${perryWitBin}/bin/perry-wit gen-types --wit wit
            fi
            echo "Perry-WIT component SDK: tsc --noEmit, perry-wit, wasmtime"
          '';
        };
      }
    );
  in systemOutputs // {
    templates.default = {
      path = ./template;
      description = "A WASI Preview 2 TypeScript component built with Perry-WIT";
    };
  };
}
