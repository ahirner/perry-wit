# Site

Perry-WIT publishes a static documentation site, developer handbook, and interactive browser playground. The playground executes the ahead-of-time compiler and runs generated WASI 0.3 WebAssembly components client-side inside Web Workers.

## Build

Build the static site distribution with Nix from the repository root:

```sh
# Build static distribution (landing page, mdBook handbook, playground, rustdoc)
nix build .#site

# Refresh local preview
nix run .#site
```

The build compiles the compiler WebAssembly component, validates compiler metadata, generates browser bindings, builds the `mdBook` developer handbook (`docs/` + generated conformance catalog) into `dist/docs/`, and bundles Rust API documentation into `dist/rust/`.

The playground loads `downloads/perry-wit.wasm` on compilation. Generated bindings read the core modules directly from byte ranges verified during the build, so the distribution contains one compiler binary. Separate Jco engine assets provide component inspection and application bindings.

## Verification

Run verification suites for compiler parity, workspace handling, and browser execution:

```sh
# Parity and workspace unit tests
node --test site/test/*.test.mjs

# Anonymous compiler export verification
node site/check-api.mjs

# Browser integration tests across Chromium and WebKit
nix develop .#browser -c node site/check-browser.mjs
```

## Examples

Interactive example packages declare their entrypoint and WIT contract in `package.json` under `perryWit`:

```json
{
  "name": "greeting",
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

The build packages bundled examples into the playground workspace. Opening a local folder in the playground imports custom TypeScript and WIT sources into memory without server transmission.

## Deployment

Continuous integration builds the site and executes verification suites on pull requests. Merges to `main` publish the generated distribution to GitHub Pages.

Successful site jobs upload a `static-site` artifact containing the complete distribution, including the landing page, `/docs/` handbook, `/play/` playground, compiler, runtime assets, and `/rust/` API documentation. Download it from the workflow run, extract it, and serve the extracted directory over HTTPS or HTTP on localhost. All asset links are relative, so the site can be served at a domain root or under a subdirectory. No build tools or application backend are needed to serve it.

The Linux `flake-build` job uploads `perry-wit-compiler` with the compiler component, WIT contract, and metadata. Tagged releases include that compiler artifact; `static-site` remains a CI download and is excluded from releases.
