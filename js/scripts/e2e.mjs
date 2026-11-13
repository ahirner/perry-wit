// End-to-end test of the published package:
// pack perry-wit (its prepack script runs the complete build) or take an already packed tarball,
// install the tarball into a fresh copy of e2e/consumer without running install scripts,
// check which files the package ships, then type-check and run the consumer.
//
// Usage: node scripts/e2e.mjs [--tarball <path>] [--keep]
// PERRY_COMPILER_WASM and the other build:wasm variables pass through to the build.
import { spawnSync } from 'node:child_process';
import { existsSync, realpathSync } from 'node:fs';
import { copyFile, cp, mkdir, mkdtemp, readdir, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, posix, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const NAME = 'e2e';
const NPM = 'npm';
// Dependencies need no install scripts, so none run.
const INSTALL_ARGS = ['install', '--ignore-scripts', '--no-audit', '--no-fund'];
const USAGE = 'Usage: node scripts/e2e.mjs [--tarball <path>] [--keep]';
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
  'dist/index.js.map',
  'dist/index.d.ts.map',
  'src/index.ts',
  'bindings/compiler.js',
  'bindings/core-modules.js',
  'bindings/compiler.core.wasm',
];

/** Top-level entries the tarball must not contain. */
const FORBIDDEN_ENTRIES = ['test', 'examples', 'scripts', 'wasm', 'node_modules', 'tsconfig.json', 'tsconfig.build.json'];

/** A failure with a message that says everything, printed without a stack trace. */
class ScriptError extends Error {}

/**
 * @typedef {object} E2eArgs
 * @property {string | undefined} tarball an already packed package to test instead of packing one (`--tarball`)
 * @property {boolean} keep keep the temporary consumer project for inspection (`--keep`)
 */

/** @param {E2eArgs} args */
async function main(args) {
  const { tarball, keep } = args;
  if (tarball && !existsSync(tarball)) throw new ScriptError(`${tarball} does not exist`);
  const directory = await mkdtemp(join(tmpdir(), 'perry-wit-e2e-'));
  try {
    const consumer = join(directory, 'consumer');
    await cp(CONSUMER_DIR, consumer, { recursive: true, filter: source => !/[\\/]node_modules$/.test(source) });
    await copyFile(tarball ?? (await pack(join(directory, 'pack'))), join(consumer, 'perry-wit.tgz'));

    run(NPM, INSTALL_ARGS, consumer);
    await checkPackageFiles(join(consumer, 'node_modules/perry-wit'));
    run(NPM, ['run', 'typecheck'], consumer);
    run(NPM, ['run', 'start'], consumer);
  } finally {
    if (keep) console.error(`${NAME}: kept ${directory}`);
    else await rm(directory, { recursive: true, force: true });
  }
}

/** Pack the package into `packDir`, which runs its complete build, and return the tarball's path. */
async function pack(packDir) {
  await mkdir(packDir, { recursive: true });
  run(NPM, ['pack', '--pack-destination', packDir], PACKAGE_DIR);
  const [tarball] = (await readdir(packDir)).filter(name => name.endsWith('.tgz'));
  if (!tarball) throw new ScriptError(`npm pack wrote no tarball to ${packDir}`);
  return join(packDir, tarball);
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
  const dangling = await danglingSourceMaps(root, files);
  if (dangling.length > 0) throw new ScriptError(`source maps name files the package does not ship: ${dangling.join(', ')}`);
  console.error(`${NAME}: the package ships ${files.length} files`);
}

/**
 * List the sources that shipped source maps and declaration maps name but the package does not ship,
 * as `map -> source` strings.
 *
 * @param {string} root the installed package
 * @param {string[]} files the files it ships, relative to `root` with `/` separators
 */
async function danglingSourceMaps(root, files) {
  const dangling = [];
  for (const file of files.filter(name => name.endsWith('.map'))) {
    const { sourceRoot = '', sources = [] } = JSON.parse(await readFile(join(root, file), 'utf8'));
    for (const source of sources) {
      if (!files.includes(posix.join(posix.dirname(file), sourceRoot, source))) dangling.push(`${file} -> ${source}`);
    }
  }
  return dangling;
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
    const { values } = parseArgs({
      options: { tarball: { type: 'string' }, keep: { type: 'boolean', default: false } },
      strict: true,
    });
    // npm runs this script from the workspace directory, so a relative path is relative to where npm was started.
    const tarball = values.tarball === undefined ? undefined : resolve(process.env.INIT_CWD ?? '.', values.tarball);
    return { tarball, keep: values.keep };
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
