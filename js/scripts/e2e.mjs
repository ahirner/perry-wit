// End-to-end test of the published package:
// pack perry-wit (its prepack script runs the complete build),
// install the tarball into a fresh copy of e2e/consumer under the workspace's supply-chain policy,
// check which files the package ships, then type-check and run the consumer.
//
// Usage: node scripts/e2e.mjs [--keep]
// PERRY_COMPILER_WASM and the other build:wasm variables pass through to the build.
import { spawnSync } from 'node:child_process';
import { realpathSync } from 'node:fs';
import { cp, mkdtemp, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const NAME = 'e2e';
const PNPM = 'pnpm';
const USAGE = 'Usage: node scripts/e2e.mjs [--keep]';
const WORKSPACE_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PACKAGE_DIR = join(WORKSPACE_DIR, 'packages/perry-wit');
const CONSUMER_DIR = join(WORKSPACE_DIR, 'e2e/consumer');

/** Files the tarball must contain, relative to the package root. */
const REQUIRED_FILES = [
  'package.json',
  'README.md',
  'dist/index.js',
  'dist/index.d.ts',
  'dist/load-core.node.js',
  'dist/load-core.browser.js',
  'bindings/compiler.js',
  'bindings/core-modules.js',
  'bindings/compiler.core.wasm',
];

/** Top-level entries the tarball must not contain. */
const FORBIDDEN_ENTRIES = ['src', 'test', 'examples', 'scripts', 'wasm', 'node_modules', 'tsconfig.json', 'tsconfig.build.json'];

/** A failure with a message that says everything, printed without a stack trace. */
class ScriptError extends Error {}

/**
 * @typedef {object} E2eArgs
 * @property {boolean} keep keep the temporary consumer project for inspection (`--keep`)
 */

/** @param {E2eArgs} args */
async function main(args) {
  const { keep } = args;
  const directory = await mkdtemp(join(tmpdir(), 'perry-wit-e2e-'));
  try {
    const packDir = join(directory, 'pack');
    run(PNPM, ['pack', '--pack-destination', packDir], PACKAGE_DIR);
    const [tarball] = (await readdir(packDir)).filter(name => name.endsWith('.tgz'));
    if (!tarball) throw new ScriptError(`pnpm pack wrote no tarball to ${packDir}`);

    const consumer = join(directory, 'consumer');
    await cp(CONSUMER_DIR, consumer, { recursive: true, filter: source => !/[\\/]node_modules$/.test(source) });
    await rename(join(packDir, tarball), join(consumer, 'perry-wit.tgz'));
    await writeFile(join(consumer, 'pnpm-workspace.yaml'), await supplyChainPolicy());

    run(PNPM, ['install'], consumer);
    await checkPackageFiles(join(consumer, 'node_modules/perry-wit'));
    run(PNPM, ['run', 'typecheck'], consumer);
    run(PNPM, ['run', 'start'], consumer);
  } finally {
    if (keep) console.error(`${NAME}: kept ${directory}`);
    else await rm(directory, { recursive: true, force: true });
  }
}

/** The workspace's pnpm settings without its package list, so the consumer installs under the same policy. */
async function supplyChainPolicy() {
  const workspace = await readFile(join(WORKSPACE_DIR, 'pnpm-workspace.yaml'), 'utf8');
  return workspace.replace(/^packages:\n(?:[ \t]+-.*\n)+/m, '');
}

/** Check that the installed package holds what it needs and nothing it should not ship. */
async function checkPackageFiles(installed) {
  const root = realpathSync(installed);
  const files = (await readdir(root, { recursive: true, withFileTypes: true }))
    .filter(entry => entry.isFile())
    .map(entry => relative(root, join(entry.parentPath, entry.name)).split('\\').join('/'));
  const missing = REQUIRED_FILES.filter(file => !files.includes(file));
  const unexpected = files.filter(file => FORBIDDEN_ENTRIES.includes(file.split('/')[0]));
  if (missing.length > 0) throw new ScriptError(`the package is missing ${missing.join(', ')}`);
  if (unexpected.length > 0) throw new ScriptError(`the package ships ${unexpected.join(', ')}`);
  console.error(`${NAME}: the package ships ${files.length} files`);
}

/** Run a command, inheriting stdio, and throw when it fails. */
function run(command, commandArgs, cwd) {
  console.error(`${NAME}: $ ${command} ${commandArgs.join(' ')}  (in ${cwd})`);
  const { status, error } = spawnSync(command, commandArgs, { cwd, stdio: 'inherit', shell: process.platform === 'win32' });
  if (error) throw error;
  if (status !== 0) throw new ScriptError(`${command} ${commandArgs.join(' ')} exited with status ${status}`);
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

/** @returns {E2eArgs | undefined} */
function parseCommandLine() {
  try {
    const { values } = parseArgs({ options: { keep: { type: 'boolean', default: false } }, strict: true });
    return { keep: values.keep };
  } catch (error) {
    console.error(`${NAME}: ${error.message}\n${USAGE}`);
    process.exitCode = 2;
    return undefined;
  }
}

if (isMain()) {
  const args = parseCommandLine();
  if (args) {
    main(args).catch(error => {
      console.error(error instanceof ScriptError ? `${NAME}: ${error.message}` : error);
      process.exitCode = 1;
    });
  }
}
