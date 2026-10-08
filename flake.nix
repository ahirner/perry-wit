{
  description = "Perry-WIT: Hermetic TypeScript compiler and WASI 0.3 component toolchain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    wasi-p3 = {
      url = "github:WebAssembly/WASI/v0.3.0";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, rust-overlay, crane, wasi-p3 }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      perSystem = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

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

        authoredSourceFilter = path: type:
          !(builtins.elem (baseNameOf path) [ ".perry" "node_modules" "target" "dist" "result" ])
          && pkgs.lib.cleanSourceFilter path type;

        # Common source filter for Rust crate builds
        commonFilter = path: type:
          authoredSourceFilter path type && (
          (pkgs.lib.hasSuffix ".wat" path) ||
          (pkgs.lib.hasSuffix ".wit" path) ||
          (pkgs.lib.hasSuffix ".ts" path) ||
          (pkgs.lib.hasSuffix ".mjs" path) ||
          (pkgs.lib.hasSuffix ".json" path) ||
          (pkgs.lib.hasSuffix ".toml" path) ||
          (pkgs.lib.hasSuffix ".lock" path) ||
          (craneLib.filterCargoSources path type));

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
            ln -sfn "${wasiP3Wit}"/* wit/deps/
          '';
        };

        # Compiler with embedded allocation-free computation helpers
        perryWitBin = craneLib.buildPackage {
          inherit src cargoArtifacts;
          pname = "perry-wit";
          version = "0.1.0";
          strictDeps = true;
          doCheck = false;
          nativeBuildInputs = with pkgs; [ pkg-config ];
          postInstall = ''
            mkdir -p "$out/lib/perry-wit-helpers"
            for f in search.wasm text.wasm fetch.wasm number.wasm json.wasm time.wasm; do
              wasm_file=$(find target -name "$f" -print -quit)
              if [ -n "$wasm_file" ]; then
                cp "$wasm_file" "$out/lib/perry-wit-helpers/"
              else
                echo "Error: could not find $f in target" >&2
                exit 1
              fi
            done
          '';
        };

        # Core WASM helper libraries
        coreHelpers = pkgs.runCommand "perry-wit-core-helpers-0.1.0" {} ''
          mkdir -p "$out/lib"
          cp ${perryWitBin}/lib/perry-wit-helpers/*.wasm "$out/lib/"
        '';

        # Example WASIp3 component hermetically compiled using perry-wit CLI and dynamic WASI WIT
        exampleMergeDocs = pkgs.stdenv.mkDerivation {
          pname = "example-merge-docs";
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin ];
          buildPhase = ''
            export WASI_WIT_PATH="${wasiP3Wit}"
            mkdir -p dist
            perry-wit examples/merge_docs.ts \
              --wit wit \
              --world command \
              -o dist/perry_merge_docs.stripped.wasm
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp dist/perry_merge_docs.stripped.wasm "$out/lib/"
          '';
        };

        # Example task component with canonical WIT exports
        exampleMergeTask = pkgs.stdenv.mkDerivation {
          pname = "example-merge-task";
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin pkgs.wasmtime ];
          buildPhase = ''
            export HOME="$TMPDIR"
            export WASMTIME_CACHE_ENABLED=false
            export WASI_WIT_PATH="${wasiP3Wit}"
            mkdir -p dist
            perry-wit examples/merge_task.ts \
              --wit wit \
              --world task-runner \
              -o dist/perry_merge_task.wasm
            wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hermetic-build")' dist/perry_merge_task.wasm
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
          src = pkgs.lib.cleanSourceWith { inherit src; filter = authoredSourceFilter; };
          nativeBuildInputs = [ perryWitBin pkgs.typescript pkgs.wasm-tools ];
          buildPhase = ''
            export WASI_WIT_PATH="${wasiP3Wit}"
            perry-wit gen-types --no-tsconfig \
              --wit ${pkgs.lib.escapeShellArg (toString wit)} \
              --entry ${pkgs.lib.escapeShellArg entry} \
              ${pkgs.lib.optionalString (world != null) ("--world " + pkgs.lib.escapeShellArg world)}
            tsc --noEmit -p .perry/types
            mkdir -p dist
            perry-wit ${pkgs.lib.escapeShellArg entry} \
              --wit ${pkgs.lib.escapeShellArg (toString wit)} \
              ${if world != null then "--world " + pkgs.lib.escapeShellArg world else ""} \
              -o "dist/${name}.wasm"
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp "dist/${name}.wasm" "$out/lib/"
          '';
        };

        mkSdkShell = { wit ? "wit", world ? null, entry ? "src/index.ts", ... }:
          pkgs.mkShell {
            name = "perry-wit-sdk";
            packages = [ perryWitBin pkgs.nodejs pkgs.typescript pkgs.wasmtime pkgs.wasm-tools ];
            WASI_WIT_PATH = wasiP3Wit;
            WASI_P3_WIT_PATH = wasiP3Wit;
            shellHook = ''
              ${perryWitBin}/bin/perry-wit gen-types --no-tsconfig \
                --wit ${pkgs.lib.escapeShellArg (toString wit)} \
                --entry ${pkgs.lib.escapeShellArg entry} \
                ${pkgs.lib.optionalString (world != null) ("--world " + pkgs.lib.escapeShellArg world)} \
                || exit "$?"
              echo "Perry-WIT SDK: node, tsc --noEmit -p .perry/types, perry-wit, wasmtime"
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
          wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("template-hello")' "${templateComponent}/lib/template-task.wasm"
          touch "$out"
        '';

        # Check: Pre-commit / CI verification that generated TypeScript SDK definitions are valid and up to date
        sdkSyncCheck = pkgs.runCommand "check-sdk-sync" {
          nativeBuildInputs = [ perryWitBin pkgs.typescript ];
        } ''
          export HOME="$TMPDIR"
          export WASI_WIT_PATH="${wasiP3Wit}"
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
          core-helpers = coreHelpers;
          helpers = coreHelpers;
          example-merge-docs = exampleMergeDocs;
          example-merge-task = exampleMergeTask;
          template-component = templateComponent;
          wasi-wit = wasiP3Wit;
          wasi-p3-wit = wasiP3Wit;
        };

        lib = {
          inherit buildComponent mkSdkShell;
        };

        checks = {
          wasi-p3-wit = checkP3Wit;
          perry-wit-fmt = craneLib.cargoFmt {
            inherit src;
          };
          inherit coreHelpers exampleMergeDocs exampleMergeTask perryWitBin templateComponent checkTemplate sdkSyncCheck;
        };

        devShells.default = pkgs.mkShell {
          name = "perry-wit-dev";
          packages = [
            toolchain
            pkgs.wasmtime
            pkgs.wasm-tools
            pkgs.nodejs
            pkgs.typescript
            pkgs.pkg-config
            pkgs.cacert
          ];

          WASI_WIT_PATH = wasiP3Wit;
          WASI_P3_WIT_PATH = wasiP3Wit;

          shellHook = ''
            if [ -d wit ] && [ ! -e wit/deps ]; then
              ln -sfn "${wasiP3Wit}" wit/deps
            fi
            echo "Perry-WIT compiler development: cargo build, cargo test"
          '';
        };

        devShells.sdk = mkSdkShell {
          entry = "examples/merge_task.ts";
          world = "task-runner";
        };

      }
    );
  in {
    packages = forAllSystems (system: perSystem.${system}.packages);
    checks = forAllSystems (system: perSystem.${system}.checks);
    devShells = forAllSystems (system: perSystem.${system}.devShells);
    lib = forAllSystems (system: perSystem.${system}.lib);
    templates.default = {
      path = ./template;
      description = "A WASI 0.3 TypeScript component built with Perry-WIT";
    };
  };
}
