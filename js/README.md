# JavaScript Workspace

`js/` is a single npm workspace covering the `perry-wit` library package and the static site:

- `packages/perry-wit` — the `perry-wit` npm package for Node.js 22+ and browsers
- `site` — the homepage, handbook build, and browser playground (`js/site/dist`)
- `e2e/consumer` — standalone consumer project used by `scripts/e2e.mjs` to test the packed tarball

`js/.npmrc` enforces `ignore-scripts=true` and `min-release-age=7` across the workspace.

## Hermetic Nix Builds

From the repository root:

```sh
nix build .#npm-package   # build, type-check, test on Node 26 & 22, and pack perry-wit-*.tgz into result/
nix run .#site            # build the full site and sync it to js/site/dist/
```

Serve `js/site/dist` over HTTP to preview the site locally:

```sh
python3 -m http.server --directory js/site/dist 8000
```

After updating `js/package-lock.json`, update `jsDeps.hash` in `flake.nix` to the hash reported by Nix.

## Local Workspace Iteration

Inside `nix develop`:

```sh
npm --prefix js ci
npm --prefix js run build       # builds wasm, Jco bindings, and TypeScript
npm --prefix js run typecheck
npm --prefix js test
npm --prefix js run test:e2e
```

Setting `PERRY_COMPILER_WASM` skips rebuilding the Wasm compiler with Cargo:

```sh
PERRY_COMPILER_WASM="$(nix build .#perry-wit-wasm --print-out-paths)/lib/perry-wit.wasm" \
  npm --prefix js run build
```

## Browser Verification

Run the headless Chromium and WebKit suite against `js/site/dist` and the bundled `perry-wit` browser loader:

```sh
nix develop .#browser -c node js/site/check-browser.mjs
```
