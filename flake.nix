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

        wasmtimeVersion = "49.0.2";
        cargoManifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        wasmtimeAsset = {
          aarch64-darwin = { target = "aarch64-macos"; hash = "sha256-6lp5JhyqOYGbhj/YUIeUKM+QVINT4eXPm8vMTnrd8sE="; };
          x86_64-darwin = { target = "x86_64-macos"; hash = "sha256-skUT7/9qXb6xluKgEJe6YyqLDqFWO6KXraJs6zqgZ9k="; };
          aarch64-linux = { target = "aarch64-linux"; hash = "sha256-yhSYjG2j2SUSvZw7rWy9BrH1dDtpEannCfGm+jBjfX8="; };
          x86_64-linux = { target = "x86_64-linux"; hash = "sha256-pNbp46WmD1J893k9Z0xIkwyAwuiXeZW4onXK0yVLkyI="; };
        }.${system};
        wasmtimeCli = assert cargoManifest.workspace.dependencies.wasmtime.version == "=${wasmtimeVersion}";
          pkgs.stdenvNoCC.mkDerivation {
            pname = "wasmtime";
            version = wasmtimeVersion;
            src = pkgs.fetchurl {
              url = "https://github.com/bytecodealliance/wasmtime/releases/download/v${wasmtimeVersion}/wasmtime-v${wasmtimeVersion}-${wasmtimeAsset.target}.tar.xz";
              inherit (wasmtimeAsset) hash;
            };
            nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.autoPatchelfHook ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.stdenv.cc.cc.lib ];
            dontBuild = true;
            dontStrip = true;
            installPhase = ''
              runHook preInstall
              install -Dm755 wasmtime "$out/bin/wasmtime"
              runHook postInstall
            '';
            doInstallCheck = true;
            installCheckPhase = ''
              "$out/bin/wasmtime" --version | grep -F "wasmtime ${wasmtimeVersion}"
            '';
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
          !(builtins.elem (baseNameOf path) [ ".perry" ".direnv" ".env" ".generated" "test-results" "node_modules" "target" "dist" "result" ])
          && pkgs.lib.cleanSourceFilter path type;

        # Common source filter for Rust crate builds
        commonFilter = path: type:
          authoredSourceFilter path type && (
          (pkgs.lib.hasSuffix ".wat" path) ||
          (pkgs.lib.hasSuffix ".wit" path) ||
          (pkgs.lib.hasSuffix ".ts" path) ||
          (pkgs.lib.hasSuffix ".mjs" path) ||
          (pkgs.lib.hasSuffix ".py" path) ||
          (pkgs.lib.hasSuffix ".json" path) ||
          (pkgs.lib.hasSuffix ".toml" path) ||
          (pkgs.lib.hasSuffix ".lock" path) ||
          (path == toString ./src/helpers/undefined.txt) ||
          (craneLib.filterCargoSources path type));

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = commonFilter;
        };
        # Shared dependency build artifacts
        cargoArtifacts = craneLib.buildDepsOnly {
          inherit src;
          pname = "perry-wit-deps";
          version = cargoManifest.package.version;
          strictDeps = true;
          doCheck = false;
          WASI_WIT_PATH = wasiP3Wit;
          nativeBuildInputs = with pkgs; [ pkg-config cacert ];
          preBuild = ''
            mkdir -p wit/deps
            ln -sfn "${wasiP3Wit}"/* wit/deps/
          '';
        };

        # Compiler with the embedded runtime and computation helpers
        perryWitBin = craneLib.buildPackage {
          inherit src cargoArtifacts;
          pname = "perry-wit";
          version = cargoManifest.package.version;
          strictDeps = true;
          doCheck = false;
          WASI_WIT_PATH = wasiP3Wit;
          nativeBuildInputs = with pkgs; [ pkg-config ];
          postBuild = ''cargo doc --locked --release --no-deps --lib -p perry-wit'';
          postInstall = ''
            mkdir -p "$out/share/doc"
            cp -r target/doc "$out/share/doc/perry-wit"
            mkdir -p "$out/lib/perry-wit-helpers"
            for f in runtime.wasm runtime-collectible.wasm std.wasm fs.wasm web.wasm search.wasm text.wasm fetch.wasm number.wasm json.wasm time.wasm; do
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

        wasmArgs = {
          inherit src;
          pname = "perry-wit-wasm";
          version = cargoManifest.package.version;
          strictDeps = true;
          doCheck = false;
          CARGO_BUILD_TARGET = "wasm32-wasip2";
          WASI_WIT_PATH = wasiP3Wit;
          PERRY_HELPER_TARGET_DIR = "target/embedded-helpers";
          cargoExtraArgs = "--bin perry-wit";
        };
        perryWitWasm = craneLib.buildPackage (wasmArgs // {
          nativeBuildInputs = [ pkgs.python3 pkgs.wasm-tools ];
          cargoArtifacts = craneLib.buildDepsOnly wasmArgs;
          installPhaseCommand = ''
            mkdir -p "$out/lib" "$out/share/perry-wit"
            python3 scripts/compiler-metadata.py target/wasm32-wasip2/release/perry-wit.wasm "$out/lib/perry-wit.wasm" \
              --revision ${pkgs.lib.escapeShellArg (self.rev or (self.dirtyRev or ("source:" + self.narHash)))} \
              > "$out/share/perry-wit/metadata.json"
            cp wit/compiler/world.wit "$out/share/perry-wit/compiler.wit"
          '';
        });

        siteSource = pkgs.lib.cleanSourceWith { src = ./.; filter = authoredSourceFilter; };
        site = pkgs.buildNpmPackage {
          pname = "perry-wit-site";
          version = cargoManifest.package.version;
          dontFixup = true;
          src = siteSource;
          npmRoot = "site";
          npmDeps = pkgs.fetchNpmDeps {
            src = pkgs.lib.cleanSourceWith { src = ./site; filter = authoredSourceFilter; };
            hash = "sha256-TtHGWMCM3i57L3BPFptDXSZvDboAWDVkdLC+MiNc9xk=";
          };
          npmFlags = [ "--ignore-scripts" ];
          nativeBuildInputs = [ wasmtimeCli pkgs.python3 pkgs.wasm-tools ];
          PERRY_COMPILER_WASM = "${perryWitWasm}/lib/perry-wit.wasm";
          PERRY_NATIVE_COMPILER = "${perryWitBin}/bin/perry-wit";
          PERRY_RUSTDOC = "${perryWitBin}/share/doc/perry-wit";
          buildPhase = ''
            runHook preBuild
            node site/build.mjs
            node --test site/test/*.test.mjs
            node site/check-api.mjs
            runHook postBuild
          '';
          installPhase = ''
            mkdir -p "$out"
            cp -r site/dist/. "$out/"
          '';
        };

        # Example task component with canonical WIT exports
        exampleMergeTask = pkgs.stdenv.mkDerivation {
          pname = "example-merge-task";
          version = "0.1.0";
          inherit src;
          nativeBuildInputs = [ perryWitBin wasmtimeCli ];
          buildPhase = ''
            export HOME="$TMPDIR"
            export WASMTIME_CACHE_ENABLED=false
            export WASI_WIT_PATH="${wasiP3Wit}"
            mkdir -p dist
            perry-wit examples/merge_task/index.ts \
              --wit wit \
              --world task-runner \
              -o dist/perry_merge_task.wasm
            wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'render-markdown("hermetic-build", ["First", "Second"])' dist/perry_merge_task.wasm
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
            packages = [ perryWitBin pkgs.nodejs pkgs.typescript wasmtimeCli pkgs.wasm-tools ];
            WASI_WIT_PATH = wasiP3Wit;
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
          nativeBuildInputs = [ wasmtimeCli ];
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
          cp ${./examples/merge_task/index.ts} work/src/merge_task.ts
          cd work
          perry-wit gen-types --wit wit --world merge-task --entry src/merge_task.ts
          tsc --noEmit
          touch "$out"
        '';

      in {
        packages = {
          default = perryWitBin;
          perry-wit = perryWitBin;
          perry-wit-wasm = perryWitWasm;
          inherit site;
          core-helpers = coreHelpers;
          example-merge-task = exampleMergeTask;
          template-component = templateComponent;
          wasi-wit = wasiP3Wit;
          wasmtime = wasmtimeCli;
        };

        apps.site = {
          type = "app";
          meta.description = "Build the site and update site/dist for the local preview";
          program = "${pkgs.writeShellApplication {
            name = "build-site";
            runtimeInputs = [ pkgs.rsync ];
            text = ''
              if [[ ! -f site/build.mjs || ! -f Cargo.toml ]]; then
                echo "Run nix run .#site from the Perry-WIT repository root." >&2
                exit 1
              fi
              if [[ -L site/dist ]]; then
                echo "site/dist must be a directory, not a symlink." >&2
                exit 1
              fi
              mkdir -p site/dist
              rsync -r --delete --chmod=Du=rwx,Dgo=rx,Fu=rw,Fgo=r ${site}/ site/dist/
              echo "Built site/dist. Refresh the existing preview to load it."
            '';
          }}/bin/build-site";
        };

        lib = {
          inherit buildComponent mkSdkShell;
        };

        checks = {
          wasi-wit = checkP3Wit;
          perry-wit-fmt = craneLib.cargoFmt {
            inherit src;
          };
          inherit coreHelpers exampleMergeTask perryWitBin perryWitWasm site templateComponent checkTemplate sdkSyncCheck;
        };

        devShells.default = pkgs.mkShell {
          name = "perry-wit-dev";
          packages = [
            toolchain
            wasmtimeCli
            pkgs.wasm-tools
            pkgs.nodejs
            pkgs.python3
            pkgs.typescript
            pkgs.pkg-config
            pkgs.cacert
          ];

          WASI_WIT_PATH = wasiP3Wit;

          shellHook = ''
            export CARGO_TARGET_DIR="$PWD/target"
            export PERRY_HELPER_TARGET_DIR="$CARGO_TARGET_DIR/embedded-helpers"
            if [ -d wit ] && [ ! -e wit/deps ]; then
              ln -sfn "${wasiP3Wit}" wit/deps
            fi
            echo "Perry-WIT compiler development: cargo build, cargo test"
          '';
        };

        devShells.sdk = mkSdkShell {
          entry = "examples/merge_task/index.ts";
          world = "task-runner";
        };

        devShells.browser = pkgs.mkShell {
          packages = [ pkgs.nodejs pkgs.playwright-driver.browsers ];
          PLAYWRIGHT_BROWSERS_PATH = pkgs.playwright-driver.browsers;
          PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        };

      }
    );
  in {
    packages = forAllSystems (system: perSystem.${system}.packages);
    apps = forAllSystems (system: perSystem.${system}.apps);
    checks = forAllSystems (system: perSystem.${system}.checks);
    devShells = forAllSystems (system: perSystem.${system}.devShells);
    lib = forAllSystems (system: perSystem.${system}.lib);
    templates.default = {
      path = ./template;
      description = "A WASI 0.3 TypeScript component built with Perry-WIT";
    };
  };
}
