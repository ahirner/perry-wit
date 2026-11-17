// Assemble third-party license notices for artifacts redistributing the compiled Wasm compiler
// or bundled JavaScript shims.
import { existsSync } from 'node:fs';
import { readdir, readFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';

/**
 * Read the repository license and embedded Rust crate notices (`Perry-MIT.txt`, `anyhow-MIT.txt`).
 *
 * @param {string} repositoryDir
 * @param {string} heading
 */
export async function compilerNotices(repositoryDir, heading) {
  const [apache, perryMit, anyhowMit] = await Promise.all([
    readFile(join(repositoryDir, 'LICENSE'), 'utf8'),
    readFile(join(repositoryDir, 'licenses/Perry-MIT.txt'), 'utf8'),
    readFile(join(repositoryDir, 'licenses/anyhow-MIT.txt'), 'utf8'),
  ]);
  return [
    heading,
    '',
    apache.trimEnd(),
    '',
    'Perry HIR (upstream revision pinned in Cargo.toml)',
    perryMit.trimEnd(),
    '',
    'anyhow (version pinned in Cargo.lock)',
    anyhowMit.trimEnd(),
    '',
  ].join('\n');
}

/**
 * Read license metadata and notice files for bundled npm packages resolved from `fromDir`.
 *
 * @param {string} fromDir
 * @param {readonly string[]} packageNames
 */
export async function bundledPackageNotices(fromDir, packageNames) {
  let notices = '';
  for (const name of packageNames) {
    const directory = findPackageDir(fromDir, name);
    const pkg = JSON.parse(await readFile(join(directory, 'package.json'), 'utf8'));
    notices += `\n${name} ${pkg.version}\n${pkg.license}\n${pkg.repository?.url || ''}\n\n`;
    for (const file of (await readdir(directory)).sort()) {
      if (/^(LICENSE|NOTICE)/i.test(file)) {
        notices += `${file}\n${await readFile(join(directory, file), 'utf8')}\n\n`;
      }
    }
  }
  return notices;
}

function findPackageDir(fromDir, packageName) {
  for (let current = resolve(fromDir); ; ) {
    const candidate = join(current, 'node_modules', packageName);
    if (existsSync(join(candidate, 'package.json'))) return candidate;
    const parent = dirname(current);
    if (parent === current) throw new Error(`Cannot find installed package ${packageName} from ${fromDir}`);
    current = parent;
  }
}
