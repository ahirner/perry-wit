# Playground

`nix build .#site` produces the complete static site. The homepage loads no scripts.
The playground uses native textareas, vanilla JavaScript, a fresh worker per operation,
and a 30-second deadline. Compiler assets load on first compilation. Cancelling terminates
the worker, including its running program.

`Open folder…` loads TypeScript and WIT into memory, preserving relative module identities.
File and entrypoint selectors appear when there are multiple choices. A single WIT world
is selected automatically; otherwise choose the interface the TypeScript implements.
Compilation discovers the available functions. WIT files in the selected file's directory
form the application package; subdirectories of `deps/` form dependency packages. Encoded
WIT packages in `deps/` are also accepted. Generated directories are skipped and source
imports are capped at 16 MiB. Reset restores the imported snapshot. Reopening rereads disk.

Examples are self-contained packages under `examples/`. Both the site build and folder
import read each `package.json`: `description` supplies the explanation, `main` selects
TypeScript, and `perryWit` supplies the WIT file, world, default export, and argument presets.
Adding a package requires no changes to the site builder. The build compiles its contents
and verifies the default export. Imported exports are discovered during compilation.

```json
{
  "name": "greeting",
  "description": "Return a greeting for the supplied name.",
  "main": "index.ts",
  "perryWit": {
    "wit": "world.wit",
    "world": "greeting",
    "export": "greet",
    "functions": {
      "greet": { "args": ["World"], "hint": "One name." }
    }
  }
}
```

`perryWit.title` optionally supplies a display name. Each function preset accepts `args`
or `urls`; URLs are resolved against the playground page to supply absolute arguments.
JSON fixture files in each bundled package are published at `examples/<package>/<file>`.
The Markdown example returns the generated source and its Unicode code point count,
including syntax and newlines. Code points are not UTF-8 bytes or grapheme clusters: an
emoji such as 🙂 is one code point, while combining accents and joined emoji use several.
The explicit string iteration also gives the same count in JavaScript runtimes.

The merge-docs presets use the live [PokéAPI](https://pokeapi.co/docs/v2) Pokémon and species
endpoints for Ditto. `Promise.all` overlaps the requests, then the JSON decoding; one guard
checks HTTP success. Network, HTTP, and JSON failures use the declared WIT error.
Native and browser tests use the checked-in JSON fixtures so CI needs no live API.
Browser fetches to other origins need CORS support.

The build adapts the pinned Preview 3 shim to pass typed-array chunks to Jco's numeric
streams. This avoids an asynchronous boundary crossing per byte on larger HTTP responses.
Browser checks include a large Unicode body; the adapter requires review when the shim changes.

Open either the whole `examples/` folder or one package. When a collection is imported,
changing the entrypoint selects that package's contract and presets. Plain folders need
no manifest; if WIT is absent, the editor starts blank and accepts a pasted contract.
Source files stay unchanged on disk. No run operation scrolls the page automatically.

# Build

For a local Cargo artifact, run inside `nix develop`:

```sh
cargo build --release --target wasm32-wasip2 --bin perry-wit
cargo doc --release --no-deps --lib -p perry-wit
npm ci --prefix site --ignore-scripts --no-audit --no-fund
mkdir -p dist
python3 scripts/compiler-metadata.py "$CARGO_TARGET_DIR/wasm32-wasip2/release/perry-wit.wasm" dist/perry-wit.wasm
PERRY_COMPILER_WASM="$PWD/dist/perry-wit.wasm" PERRY_RUSTDOC="$CARGO_TARGET_DIR/doc" node site/build.mjs
node --test site/test/*.test.mjs
node site/check-api.mjs
```

The build generates bindings from that compiler component with pinned Jco. The Preview 3
shim runs generated application components. The compiler uses upstream dependencies and
standard WASI 0.2 imports, with no preopened directories. The browser provides an empty
environment and captures compiler stdout/stderr in diagnostics. Native callers retain
upstream environment settings and output. Application imports outside the supported browser
interfaces produce diagnostics while the component remains downloadable.

The compiler package and WIT API are version `0.1.0`. Component output is one optional
record containing required full and stripped buffers; core bytes and `allocation-report`
are always returned. Dependency module identifiers are required. `compile-core` uses raw
exports without WIT, while `compile` with `core-only` still targets its WIT world.

`scripts/compiler-metadata.py` uses the pinned `wasm-tools metadata add` implementation
of `wasm-metadata::AddMetadata`. It annotates only the outer compiler component, preserving
existing producer entries. Name, description, version, repository, homepage, Apache-2.0
license, and author come from Cargo.toml. Rust and the actual rustc/wasm-tools versions
describe the build tools; TypeScript is the compiler's input, not its implementation language.
Local revisions use the Git HEAD with a `-dirty` suffix when source changes or untracked
files exist. Nix supplies its clean/dirty revision, or a source NAR hash for unversioned
source inputs. Metadata collection happens after linking, outside library execution.

The site rejects missing or mismatched compiler metadata and checks the extracted WIT
exports before generating browser bindings. Downloads, bindings, and releases originate
from this same annotated artifact. `downloads/compiler.metadata.json` records its inspected
outer metadata. The site also publishes Rust API documentation and the interface declaration
archive. The script-free BUILD section groups CLI, Rust, and the downloadable compiler component,
with one Nix build command and WIT as the compiler API reference. Generated JavaScript
declarations remain in the distribution archive. Distribution notices cover the Wasm compiler
and browser runtime; they are linked beside the Wasm download. Both pages render the same
footer template with the package version from the verified compiler metadata.

`sizes.json` reports uncompressed HTML/CSS, UI JavaScript, compiler, worker, and Jco engines
separately. The build enforces a combined 10 KiB budget for both pages and shared CSS.
No remote fonts, editor framework, CDN, or service worker is required.

# Checks

```sh
nix develop .#browser -c env SITE_OUTPUT=result node site/check-browser.mjs
```

Browser checks use Playwright Chromium and WebKit. They route static files from the build
to an HTTPS repository-path URL in the browser; they do not start or modify a server.
For existing browser installations, set `CHROMIUM_EXECUTABLE` and `WEBKIT_EXECUTABLE`.
Screenshots are written to `site/test-results/`.

`check-api.mjs` calls anonymous byte-based Wasm exports with host capabilities denied.
Set `PERRY_NATIVE_COMPILER` and `PERRY_COMPILER_WASM` to additionally compare native CLI,
Wasm CLI, core outputs, and generated SDK bytes. Wasmtime must be on `PATH`.

# Publishing

CI builds and checks the compiler and site on pull requests. Successful `main` builds
deploy through GitHub Pages artifact/deployment actions. Pages write and OIDC permissions
are confined to the deployment job. Tags publish the compiler, WIT, and declaration archive.

Set the repository's Pages source to **GitHub Actions** and homepage to
`https://ahirner.github.io/perry-wit/`. Relative asset URLs also support a future custom
domain without rebuilding. The standard repository URL requires no DNS setup.
