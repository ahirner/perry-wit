// Prepare the bindings `jco transpile` wrote for publishing:
// list their core modules with static `new URL('./name', import.meta.url)` references,
// which bundlers recognize and copy, and drop Jco's default module loader,
// whose dynamic URL would make bundlers copy every file in the directory.
// Running it again on prepared bindings only refreshes the module list.
//
// Usage: node scripts/finish-bindings.mjs [--bindings <dir>]
import { realpathSync } from 'node:fs';
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const NAME = 'finish-bindings';
const USAGE = `Usage: node scripts/${NAME}.mjs [--bindings <dir>]`;
const PACKAGE_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_LOADER = /^if \(!getCoreModule\) getCoreModule = .*import\.meta\.url\)\);\n/m;
const DEFAULT_LOADER_REMOVED = `// ${NAME}: removed Jco's default core module loader; callers pass getCoreModule.\n`;

/** A failure with a message that says everything, printed without a stack trace. */
class ScriptError extends Error {}

/**
 * @typedef {object} FinishBindingsArgs
 * @property {string} bindings the directory `jco transpile` wrote to (`--bindings`)
 */

/** @param {FinishBindingsArgs} args */
async function main(args) {
  const { bindings } = args;
  const files = await readdir(bindings).catch(error => {
    if (error.code === 'ENOENT') throw new ScriptError(`${bindings} does not exist; run build:bindings first.`);
    throw error;
  });
  const modules = files.filter(name => name.endsWith('.wasm')).sort();
  if (modules.length === 0) throw new ScriptError(`${bindings} has no core modules; run build:bindings first.`);
  if (!files.includes('compiler.js')) throw new ScriptError(`${bindings} has no compiler.js; run build:bindings first.`);

  const entries = modules.map(name => `  ${JSON.stringify(name)}: new URL(${JSON.stringify(`./${name}`)}, import.meta.url),`);
  await writeFile(join(bindings, 'core-modules.js'), `export const coreModules = {\n${entries.join('\n')}\n};\n`);
  await writeFile(join(bindings, 'core-modules.d.ts'), 'export declare const coreModules: Readonly<Record<string, URL>>;\n');

  const compiler = join(bindings, 'compiler.js');
  const source = await readFile(compiler, 'utf8');
  if (DEFAULT_LOADER.test(source)) {
    await writeFile(compiler, source.replace(DEFAULT_LOADER, DEFAULT_LOADER_REMOVED));
  } else if (source.includes(DEFAULT_LOADER_REMOVED)) {
    console.error(`${NAME}: ${compiler} is already prepared; refreshed the core module list only.`);
  } else {
    throw new ScriptError(`${compiler} has no default core module loader to remove; review the Jco output after updating Jco.`);
  }
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

/** @returns {FinishBindingsArgs | undefined} */
function parseCommandLine() {
  try {
    const { values } = parseArgs({
      options: { bindings: { type: 'string', default: join(PACKAGE_DIR, 'bindings') } },
      strict: true,
    });
    return { bindings: resolve(values.bindings) };
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
