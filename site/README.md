# Site

Static documentation site, handbook, and browser playground for Perry-WIT. The playground runs the compiler component and executes generated WASI 0.3 components inside Web Workers.

## Build

```sh
# Build static distribution (landing page, mdBook handbook, playground, rustdoc)
nix build .#site

# Refresh local preview
nix run .#site
```

Generated browser bindings slice core modules directly from verified byte ranges inside `downloads/perry-wit.wasm`, shipping a single compiler binary alongside the Jco component inspection and transpile engines.

## Verification

```sh
# Parity and workspace unit tests
node --test site/test/*.test.mjs

# Anonymous compiler export verification
node site/check-api.mjs

# Browser integration tests across Chromium and WebKit
nix develop .#browser -c node site/check-browser.mjs
```

## Examples

Playground example packages declare their entrypoint and WIT contract in `package.json` under `perryWit`:

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

## Artifacts

CI produces two artifacts:

- `static-site`: self-contained site distribution with relative asset paths, published to GitHub Pages on merges to `main`.
- `perry-wit-compiler`: compiler component, WIT contract, and metadata, attached to tagged releases.
