const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const skipped = new Set(['.git', '.direnv', '.perry', 'target', 'node_modules', 'dist', 'build', 'vendor']);
export const encode = text => encoder.encode(text);
export const decode = bytes => decoder.decode(bytes);
export const directory = name => name.includes('/') ? name.slice(0, name.lastIndexOf('/')) : '';

export function worldNames(source) {
  return [...source.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g, '').matchAll(/\bworld\s+%?([\w-]+)/g)].map(match => match[1]);
}

export async function readFolder(selected) {
  const files = new Map();
  let total = 0;
  for (const file of selected) {
    const parts = (file.webkitRelativePath || file.name).split('/');
    if (file.webkitRelativePath) parts.shift();
    if (parts.some(part => skipped.has(part))) continue;
    if (parts.some(part => !part || part === '.' || part === '..')) throw new Error('Invalid folder entry.');
    const name = parts.join('/');
    if (!/\.(ts|wit|wasm|json)$/.test(name)) continue;
    total += file.size;
    if (total > 16 * 1024 * 1024) throw new Error('Selected source files exceed 16 MiB. Choose a smaller folder.');
    if (files.has(name)) throw new Error(`Duplicate file: ${name}`);
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (/\.(ts|wit)$/.test(name)) {
      try { decode(bytes); } catch { throw new Error(`${name} is not valid UTF-8 text. Save it as UTF-8 and open the folder again.`); }
    }
    files.set(name, bytes);
  }
  if (![...files.keys()].some(name => /(?<!\.d)\.ts$/.test(name))) throw new Error('No TypeScript entrypoint found.');
  return files;
}

export function compileRequest(files, entry, witFile, world) {
  if (!files.has(entry)) throw new Error('Select a TypeScript entrypoint.');
  if (witFile && !files.has(witFile)) throw new Error('The selected WIT package is missing.');
  const root = directory(witFile || '');
  const dependencyRoot = root ? `${root}/deps/` : 'deps/';
  const packages = new Map();
  const encoded = [];
  const application = [];
  const modules = [];
  for (const [name, contents] of [...files].sort(([a], [b]) => a.localeCompare(b))) {
    if (name.endsWith('.ts') && name !== entry) modules.push({ name, contents });
    if (name.endsWith('.wit')) {
      const source = { name, contents };
      if (directory(name) === root) application.push(source);
      else if (name.startsWith(dependencyRoot)) {
        const group = directory(name);
        if (!packages.has(group)) packages.set(group, []);
        packages.get(group).push(source);
      }
    } else if (name.startsWith(dependencyRoot) && name.endsWith('.wasm')) encoded.push({ tag: 'encoded', val: contents });
  }
  return {
    program: { source: { name: entry, contents: files.get(entry) }, modules },
    wit: witFile ? { application: { tag: 'sources', val: application }, dependencies: [...packages.values()].map(val => ({ tag: 'sources', val })).concat(encoded) } : undefined,
    options: { world: world || undefined, coreOnly: false },
  };
}

/** Read component settings and descriptions from supplied package.json files. */
export function packagePrograms(files) {
  const programs = [];
  for (const [name, bytes] of files) {
    if (name.split('/').at(-1) !== 'package.json') continue;
    let pkg;
    try { pkg = JSON.parse(decode(bytes)); } catch { throw new Error(`${name}: invalid package.json`); }
    if (!pkg || typeof pkg !== 'object' || Array.isArray(pkg)) throw new Error(`${name}: expected a package object`);
    const root = directory(name);
    const path = value => {
      if (typeof value !== 'string' || value.startsWith('/') || value.includes('\\')) throw new Error(`${name}: expected a relative file path`);
      const parts = value.split('/').filter(part => part !== '.');
      if (parts.some(part => !part || part === '..')) throw new Error(`${name}: file paths must stay inside the package`);
      return [root, ...parts].filter(Boolean).join('/');
    };
    const settings = pkg.perryWit || {};
    const entry = pkg.main ? path(pkg.main) : ['src/index.ts', 'index.ts'].map(path).find(file => files.has(file));
    const wit = settings.wit ? path(settings.wit) : ['wit/world.wit', 'world.wit'].map(path).find(file => files.has(file));
    if (!pkg.perryWit && !entry?.endsWith('.ts')) continue;
    if (!entry || !files.has(entry)) throw new Error(`${name}: TypeScript entrypoint ${entry || 'main'} is missing from the selected folder`);
    if (settings.wit && !files.has(wit)) throw new Error(`${name}: WIT contract ${wit || 'perryWit.wit'} is missing from the selected folder`);
    if (typeof pkg.name !== 'string' || !pkg.name) throw new Error(`${name}: a package name is required`);
    if (pkg.description !== undefined && typeof pkg.description !== 'string') throw new Error(`${name}: description must be text`);
    const functions = settings.functions || {};
    if (typeof functions !== 'object' || Array.isArray(functions)) throw new Error(`${name}: functions must contain named argument presets`);
    for (const [fn, input] of Object.entries(functions)) {
      if (!input || (input.args !== undefined && !Array.isArray(input.args)) || (input.urls !== undefined && (!Array.isArray(input.urls) || input.urls.some(url => typeof url !== 'string'))) || (input.args !== undefined && input.urls !== undefined)) throw new Error(`${name}: ${fn} needs an args array or a urls array`);
    }
    if (settings.export && !functions[settings.export]) throw new Error(`${name}: no argument preset for ${settings.export}`);
    programs.push({ id: pkg.name, title: settings.title || pkg.name, description: pkg.description || '', root, entry, wit, world: settings.world, export: settings.export, functions });
  }
  if (new Set(programs.map(program => program.id)).size !== programs.length) throw new Error('Selected packages must have distinct names');
  return programs.sort((a, b) => a.title.localeCompare(b.title));
}
