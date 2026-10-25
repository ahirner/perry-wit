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

        # Node.js 26 enables Temporal by default, which the conformance oracle compares against.
        nodejs = pkgs.nodejs_26;
        wasmtimeVersion = "50.0.0";
        cargoManifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        wasmtimeAsset = {
          aarch64-darwin = { target = "aarch64-macos"; hash = "sha256-Mj3AmNwEVdYPOWxARifpZ7cDP/z6Kav/+yQxJs+CydY="; };
          x86_64-darwin = { target = "x86_64-macos"; hash = "sha256-rRTZ+2pR0WMzGEJtbfIXNBJ/rdyB/hijr9cMWWTqvrU="; };
          aarch64-linux = { target = "aarch64-linux"; hash = "sha256-EPmtH4U50uBOBBzGIt5qJwEJqx5Dpr4faYQ8IIIWaDI="; };
          x86_64-linux = { target = "x86_64-linux"; hash = "sha256-d8Ge4EN5kfmseDe5wmnZxwdnRPhdrSY/6lIUzCcjx/8="; };
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
        cargoLock = builtins.fromTOML (builtins.readFile ./Cargo.lock);
        perryLock = pkgs.lib.findFirst (package: package.name == "perry-hir")
          (throw "Cargo.lock must contain perry-hir") cargoLock.package;
        perryRevision = pkgs.lib.last (pkgs.lib.splitString "#" perryLock.source);
        perrySource = (craneLib.downloadCargoPackageFromGit {
          git = builtins.head (pkgs.lib.splitString "?" (pkgs.lib.removePrefix "git+" perryLock.source));
          rev = perryRevision;
        }).src;

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

        cleanSource = source: pkgs.lib.cleanSourceWith {
          src = source;
          filter = pkgs.nix-gitignore.gitignoreFilterPure pkgs.lib.cleanSourceFilter
            (pkgs.lib.optional (builtins.pathExists (source + "/.gitignore")) (source + "/.gitignore"))
            source;
        };
        repoSource = cleanSource ./.;

        # Common source filter for Rust crate builds
        commonFilter = path: type:
          path != toString ./scripts && (
          (pkgs.lib.hasSuffix ".wat" path) ||
          (pkgs.lib.hasSuffix ".wit" path) ||
          (pkgs.lib.hasSuffix ".ts" path) ||
          (pkgs.lib.hasSuffix ".mjs" path) ||
          (pkgs.lib.hasSuffix ".json" path) ||
          (pkgs.lib.hasSuffix ".toml" path) ||
          (pkgs.lib.hasSuffix ".lock" path) ||
          (path == toString ./runtime/undefined.txt) ||
          (craneLib.filterCargoSources path type));

        src = pkgs.lib.cleanSourceWith {
          src = repoSource;
          filter = commonFilter;
        };
        cargoVendorDir = craneLib.vendorMultipleCargoDeps {
          cargoLockList = [ ./Cargo.lock ./runtime/Cargo.lock ];
        };
        compilerArgs = {
          inherit src cargoVendorDir;
          pname = "perry-wit";
          version = cargoManifest.package.version;
          strictDeps = true;
          doCheck = false;
          WASI_WIT_PATH = wasiP3Wit;
          nativeBuildInputs = [ pkgs.pkg-config ];
        };

        # Shared dependency build artifacts
        cargoArtifacts = craneLib.buildDepsOnly (compilerArgs // {
          pname = "perry-wit-deps";
          nativeBuildInputs = with pkgs; [ pkg-config cacert ];
          preBuild = ''
            mkdir -p wit/deps
            ln -sfn "${wasiP3Wit}"/* wit/deps/
          '';
        });

        # Compiler with the embedded runtime artifacts
        perryWitBin = craneLib.buildPackage (compilerArgs // {
          inherit cargoArtifacts;
          postBuild = ''cargo doc --locked --release --no-deps --lib -p perry-wit'';
          postInstall = ''
            mkdir -p "$out/share/doc"
            cp -r target/doc "$out/share/doc/perry-wit"
            mkdir -p "$out/lib/perry-wit-runtime"
            runtime_marker=$(find target -path '*/build/perry-wit-*/out/runtime/runtime.wasm' -print -quit)
            test -n "$runtime_marker"
            cp "$(dirname "$runtime_marker")"/*.wasm "$out/lib/perry-wit-runtime/"
          '';
        });

        # Precompiled Wasm runtime artifacts
        runtimeLibraries = pkgs.runCommand "perry-wit-runtime-libraries-0.1.0" {} ''
          mkdir -p "$out/lib"
          cp ${perryWitBin}/lib/perry-wit-runtime/*.wasm "$out/lib/"
        '';

        muslTarget = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
        staticCraneLib = craneLib.overrideToolchain (toolchain.override {
          targets = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.targets
            ++ [ muslTarget ];
        });
        staticArgs = compilerArgs // {
          pname = "perry-wit-static";
          CARGO_BUILD_TARGET = muslTarget;
          CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
          cargoExtraArgs = "--locked --bin perry-wit";
        };
        perryWitStatic = staticCraneLib.buildPackage (staticArgs // {
          cargoArtifacts = staticCraneLib.buildDepsOnly staticArgs;
          PERRY_PREBUILT_RUNTIME_DIR = "${runtimeLibraries}/lib";
        });

        perryWitRelease = if pkgs.stdenv.hostPlatform.isLinux then perryWitStatic
          else pkgs.runCommand "perry-wit-release-${cargoManifest.package.version}" {
            nativeBuildInputs = [ pkgs.darwin.cctools ];
          } ''
            mkdir -p "$out/bin"
            cp ${perryWitBin}/bin/perry-wit "$out/bin/"
            chmod u+w "$out/bin/perry-wit"
            install_name_tool -change "${pkgs.lib.getLib pkgs.libiconv}/lib/libiconv.2.dylib" \
              /usr/lib/libiconv.2.dylib "$out/bin/perry-wit"
            source ${pkgs.darwin.signingUtils}
            sign "$out/bin/perry-wit"
          '';

        wasmArgs = compilerArgs // {
          pname = "perry-wit-wasm";
          CARGO_BUILD_TARGET = "wasm32-wasip2";
          cargoExtraArgs = "--bin perry-wit";
        };
        perryWitWasm = craneLib.buildPackage (wasmArgs // {
          nativeBuildInputs = [ pkgs.python3 pkgs.wasm-tools ];
          cargoArtifacts = craneLib.buildDepsOnly wasmArgs;
          PERRY_PREBUILT_RUNTIME_DIR = "${runtimeLibraries}/lib";
          installPhaseCommand = ''
            mkdir -p "$out/lib" "$out/share/perry-wit" scripts
            cp ${./scripts/compiler-metadata.py} scripts/compiler-metadata.py
            python3 scripts/compiler-metadata.py target/wasm32-wasip2/release/perry-wit.wasm "$out/lib/perry-wit.wasm" \
              --revision ${pkgs.lib.escapeShellArg (self.rev or (self.dirtyRev or ("source:" + self.narHash)))} \
              > "$out/share/perry-wit/metadata.json"
            cp wit/compiler/world.wit "$out/share/perry-wit/compiler.wit"
          '';
        });

        conformanceDoc = pkgs.runCommand "perry-conformance-doc" {
          nativeBuildInputs = [ toolchain ];
        } ''
          rustc --edition=2024 --check-cfg 'cfg(conformance_doc_bin)' --cfg conformance_doc_bin ${./crates/conformance}/src/catalog.rs -o conformance-doc
          ./conformance-doc > "$out"
        '';

        site = pkgs.buildNpmPackage {
          pname = "perry-wit-site";
          inherit nodejs;
          version = cargoManifest.package.version;
          dontFixup = true;
          src = repoSource;
          npmRoot = "site";
          npmDeps = pkgs.fetchNpmDeps {
            src = "${repoSource}/site";
            hash = "sha256-HVyTUkc1wePoDWyDEgC34ozTNiFK/mJJWBqPXVPTDzs=";
          };
          npmFlags = [ "--ignore-scripts" ];
          nativeBuildInputs = [ wasmtimeCli pkgs.python3 pkgs.wasm-tools pkgs.mdbook ];
          PERRY_COMPILER_WASM = "${perryWitWasm}/lib/perry-wit.wasm";
          PERRY_NATIVE_COMPILER = "${perryWitBin}/bin/perry-wit";
          PERRY_RUSTDOC = "${perryWitBin}/share/doc/perry-wit";
          PERRY_CONFORMANCE_DOC = conformanceDoc;
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
              --wit examples/merge_task \
              --world merge-task \
              -o dist/perry_merge_task.wasm
            wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'render-markdown("hermetic-build", ["First", "Second"])' dist/perry_merge_task.wasm
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp dist/perry_merge_task.wasm "$out/lib/"
          '';
        };

        # Build components declaratively for consumers
        buildComponent = {
          name ? "component",
          src,
          entry ? "src/index.ts",
          wit ? null,
          world ? null,
        }: pkgs.stdenv.mkDerivation {
          pname = name;
          version = "0.1.0";
          src = cleanSource src;
          nativeBuildInputs = [ perryWitBin pkgs.typescript pkgs.wasm-tools ];
          buildPhase = ''
            export WASI_WIT_PATH="${wasiP3Wit}"
            perry-wit gen-types --no-tsconfig \
              ${pkgs.lib.optionalString (wit != null) ("--wit " + pkgs.lib.escapeShellArg (toString wit))} \
              --entry ${pkgs.lib.escapeShellArg entry} \
              ${pkgs.lib.optionalString (world != null) ("--world " + pkgs.lib.escapeShellArg world)}
            tsc --noEmit -p .perry/types
            mkdir -p dist
            perry-wit ${pkgs.lib.escapeShellArg entry} \
              ${pkgs.lib.optionalString (wit != null) ("--wit " + pkgs.lib.escapeShellArg (toString wit))} \
              ${if world != null then "--world " + pkgs.lib.escapeShellArg world else ""} \
              -o "dist/${name}.wasm"
          '';
          installPhase = ''
            mkdir -p "$out/lib"
            cp "dist/${name}.wasm" "$out/lib/"
          '';
        };

        mkSdkShell = { wit ? null, world ? null, entry ? "src/index.ts", ... }:
          pkgs.mkShell {
            name = "perry-wit-sdk";
            packages = [ perryWitBin nodejs pkgs.typescript wasmtimeCli pkgs.wasm-tools ];
            WASI_WIT_PATH = wasiP3Wit;
            shellHook = ''
              ${perryWitBin}/bin/perry-wit gen-types --no-tsconfig \
                ${pkgs.lib.optionalString (wit != null) ("--wit " + pkgs.lib.escapeShellArg (toString wit))} \
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
          cp ${./examples/merge_task/world.wit} work/wit/
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
          perry-wit-release = perryWitRelease;
          perry-wit-wasm = perryWitWasm;
          inherit site;
          runtime-libraries = runtimeLibraries;
          example-merge-task = exampleMergeTask;
          template-component = templateComponent;
          wasi-wit = wasiP3Wit;
          wasmtime = wasmtimeCli;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          perry-wit-static = perryWitStatic;
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

        apps.bench-perry = {
          type = "app";
          meta.description = "Compare locked upstream Perry benchmarks in Node and Wasmtime";
          program = "${pkgs.writeShellApplication {
            name = "bench-perry";
            runtimeInputs = [ perryWitBin nodejs wasmtimeCli pkgs.python3 ];
            text = ''
              export BENCH_PERRY_SOURCE=${perrySource}
              export BENCH_PERRY_REVISION=${perryRevision}
              export WASI_WIT_PATH=${wasiP3Wit}
              exec python3 ${./scripts/bench-perry.py}
            '';
          }}/bin/bench-perry";
        };

        lib = {
          inherit buildComponent mkSdkShell;
        };

        checks = {
          wasi-wit = checkP3Wit;
          perry-wit-fmt = craneLib.cargoFmt {
            inherit src;
            cargoExtraArgs = "--all";
          };
          perry-runtime-fmt = craneLib.cargoFmt {
            inherit src;
            cargoExtraArgs = "--all --manifest-path runtime/Cargo.toml";
          };
          inherit runtimeLibraries exampleMergeTask perryWitBin perryWitWasm site templateComponent checkTemplate sdkSyncCheck;
        };

        devShells.default = pkgs.mkShell {
          name = "perry-wit-dev";
          packages = [
            toolchain
            wasmtimeCli
            pkgs.wasm-tools
            nodejs
            pkgs.python3
            pkgs.typescript
            pkgs.mdbook
            pkgs.pkg-config
            pkgs.cacert
          ];

          WASI_WIT_PATH = wasiP3Wit;

          shellHook = ''
            export CARGO_TARGET_DIR="$PWD/target"
            export PERRY_RUNTIME_TARGET_DIR="$CARGO_TARGET_DIR/embedded-runtime"
            if [ -d wit ] && [ ! -e wit/deps ]; then
              ln -sfn "${wasiP3Wit}" wit/deps
            fi
            echo "Perry-WIT compiler development: cargo build, cargo test"
          '';
        };

        devShells.sdk = mkSdkShell {
          entry = "examples/merge_task/index.ts";
          wit = "examples/merge_task";
          world = "merge-task";
        };

        devShells.browser = pkgs.mkShell {
          packages = [ nodejs pkgs.playwright-driver.browsers ]
            # Headless WPE WebKit still needs an EGL provider on Linux.
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.mesa.llvmpipeHook ];
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
