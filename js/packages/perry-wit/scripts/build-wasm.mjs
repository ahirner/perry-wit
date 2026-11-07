// Produce wasm/perry-wit.wasm, the compiler component the bindings are generated from.
// PERRY_COMPILER_WASM names a prebuilt component (for example from `nix build .#perry-wit-wasm`);
// otherwise Cargo builds it from this repository.
//
// Usage: node scripts/build-wasm.mjs [--output <path>]
import { spawnSync } from 'node:child_process';
import { realpathSync } from 'node:fs';
import { copyFile, mkdir } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const TARGET = 'wasm32-wasip2';
const PACKAGE_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const REPOSITORY_DIR = resolve(PACKAGE_DIR, '../../..');

/**
 * @typedef {object} BuildWasmArgs
 * @property {string} output where to write the compiler component (`--output`)
 * @property {string | undefined} prebuilt an existing component to copy instead of building one (`PERRY_COMPILER_WASM`)
 * @property {string} cargo the Cargo executable (`CARGO`)
 * @property {string} cargoTargetDir Cargo's target directory (`CARGO_TARGET_DIR`)
 */

/** @param {BuildWasmArgs} args */
async function main(args) {
  const { output, prebuilt, cargo, cargoTargetDir } = args;
  let input = prebuilt;
  if (!input) {
    const cargoArgs = ['build', '--release', '--locked', '--target', TARGET, '--bin', 'perry-wit', '--manifest-path', join(REPOSITORY_DIR, 'Cargo.toml')];
    console.error(`$ ${cargo} ${cargoArgs.join(' ')}`);
    const { status, error } = spawnSync(cargo, cargoArgs, { stdio: 'inherit' });
    if (error) throw error;
    if (status !== 0) throw new Error(`${cargo} build exited with status ${status}`);
    input = join(cargoTargetDir, TARGET, 'release/perry-wit.wasm');
  }
  await mkdir(dirname(output), { recursive: true });
  await copyFile(input, output);
  console.error(`${input} -> ${output}`);
}

/** Whether this module is the entry point, on runtimes with and without `import.meta.main`. */
function isMain() {
  if (typeof import.meta.main === 'boolean') return import.meta.main;
  if (!process.argv[1]) return false;
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}

if (isMain()) {
  const { values } = parseArgs({
    options: { output: { type: 'string', default: join(PACKAGE_DIR, 'wasm/perry-wit.wasm') } },
    strict: true,
  });
  /** @type {BuildWasmArgs} */
  const args = {
    output: resolve(values.output),
    prebuilt: process.env.PERRY_COMPILER_WASM || undefined,
    cargo: process.env.CARGO || 'cargo',
    cargoTargetDir: process.env.CARGO_TARGET_DIR || join(REPOSITORY_DIR, 'target'),
  };
  main(args).catch(error => {
    console.error(error);
    process.exitCode = 1;
  });
}
