import { readFile, writeFile, mkdir, cp, rm, readdir, stat } from 'node:fs/promises';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { build } from 'esbuild';
import { transpile } from '@bytecodealliance/jco';
import { compilerHost } from './play/compiler-host.js';
import { compileRequest, decode, packagePrograms } from './play/workspace.js';

const root = dirname(fileURLToPath(import.meta.url));
const repository = dirname(root);
const compiler = process.env.PERRY_COMPILER_WASM;
if (!compiler) throw new Error('Set PERRY_COMPILER_WASM to the combined wasm32-wasip2 compiler artifact.');
const rustdoc = process.env.PERRY_RUSTDOC;
if (!rustdoc) throw new Error('Set PERRY_RUSTDOC to the generated Rust API documentation.');
const metadata = execFileSync('python3', [join(repository, 'scripts/compiler-metadata.py'), compiler], { encoding: 'utf8' });
const packageVersion = JSON.parse(metadata).version;
const contract = await readFile(join(repository, 'wit/compiler/world.wit'), 'utf8');
const packageId = contract.match(/^package ([^;]+);/)[1];
const extracted = execFileSync('wasm-tools', ['component', 'wit', compiler], { encoding: 'utf8' });
const [packageName, apiVersion] = packageId.split('@');
if (apiVersion !== packageVersion) throw new Error('Compiler package and WIT API versions must match.');
if (!extracted.includes(`export ${packageName}/api@${apiVersion}`) || !extracted.includes('export wasi:cli/run@0.2.')) throw new Error('Compiler artifact must export the current API and CLI.');
const output = resolve(process.env.SITE_OUTPUT || join(root, 'dist'));
const generated = join(root, '.generated');
await mkdir(generated, { recursive: true });
await rm(output, { recursive: true, force: true });
await mkdir(join(output, 'play/assets/engine'), { recursive: true });
await mkdir(join(output, 'play/assets/compiler'), { recursive: true });
await mkdir(join(output, 'downloads'), { recursive: true });
for (const file of ['index.html', 'style.css', 'play/index.html', 'play/app.js', 'play/workspace.js']) await cp(join(root, file), join(output, file));
const footer = (await readFile(join(root, 'footer.html'), 'utf8')).trim();
for (const [page, path] of [['index.html', './'], ['play/index.html', '../']]) {
  const html = await readFile(join(output, page), 'utf8');
  await writeFile(join(output, page), html.replace('<!-- footer -->', footer.replaceAll('{{root}}', path).replaceAll('{{version}}', packageVersion)));
}
await cp(compiler, join(output, 'downloads/perry-wit.wasm'));
await cp(join(repository, 'wit/compiler/world.wit'), join(output, 'downloads/compiler.wit'));
await writeFile(join(output, 'downloads/compiler.metadata.json'), metadata);
await cp(rustdoc, join(output, 'rust'), { recursive: true });
await writeFile(join(output, '.nojekyll'), '');

const bindings = await transpile(await readFile(compiler), { name: 'compiler', instantiation: 'async', base64Cutoff: 0, nodejsCompat: false });
for (const [name, contents] of Object.entries(bindings.files)) {
  const target = join(generated, 'compiler', name);
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, contents);
  if (name.endsWith('.wasm')) await writeFile(join(output, 'play/assets/compiler', name), contents);
  if (name.endsWith('.d.ts')) {
    const destination = join(output, 'downloads', name);
    await mkdir(dirname(destination), { recursive: true });
    await writeFile(destination, contents);
  }
}
await writeFile(join(generated, 'compiler-imports.js'), `export const imports = ${JSON.stringify(bindings.imports)};\n`);
execFileSync('tar', ['-czf', join(output, 'downloads/perry-wit-interfaces.tar.gz'), '-C', join(output, 'downloads'), 'compiler.d.ts', 'interfaces']);

const vendor = dirname(fileURLToPath(import.meta.resolve('@bytecodealliance/jco-transpile/component')));
for (const name of await readdir(vendor)) if (name.endsWith('.wasm')) await cp(join(vendor, name), join(output, 'play/assets/engine', name));
await build({
  entryPoints: [join(root, 'play/worker.js')], outfile: join(output, 'play/assets/worker.js'),
  bundle: true, format: 'esm', platform: 'browser', target: 'es2022', minify: true,
  alias: { '@perry/wasm-tools': join(vendor, 'wasm-tools.js') },
  plugins: [{ name: 'engine-assets', setup(build) {
    build.onLoad({ filter: /preview3-shim\/dist\/browser\/streams\.js$/ }, async ({ path }) => {
      const source = await readFile(path, 'utf8');
      const perByte = /for \(const byte of bytes\) \{\s*controller\.enqueue\(byte\);\s*\}/;
      if (!perByte.test(source)) throw new Error('Review the Preview 3 byte-stream adapter after updating the shim.');
      // Jco accepts typed-array chunks for numeric streams, avoiding one async event per byte.
      return { contents: source.replace(perByte, 'controller.enqueue(bytes);'), loader: 'js' };
    });
    build.onLoad({ filter: /vendor\/(wasm-tools|js-component-bindgen-component)\.js$/ }, async ({ path }) => ({
      contents: (await readFile(path, 'utf8')).replace(/if \(isNode\) \{[\s\S]*?return WebAssembly\.compile\(await _fs\.readFile\(url\)\);\n  \}/, '').replace(/new URL\('\.\/([^']+\.wasm)', import\.meta\.url\)/g, "new URL('./engine/$1' + new URL(import.meta.url).search, import.meta.url)"), loader: 'js',
    }));
  } }],
});

const { instantiate } = await import(new URL('./.generated/compiler/compiler.js', import.meta.url));
const { api } = await instantiate(async name => WebAssembly.compile(await readFile(join(generated, 'compiler', name))), compilerHost(bindings.imports));
const exampleFiles = new Map();
for (const name of await readdir(join(repository, 'examples'), { recursive: true })) {
  if (/\.(ts|wit|json|wasm)$/.test(name)) exampleFiles.set(name, new Uint8Array(await readFile(join(repository, 'examples', name))));
}
const examples = [];
for (const program of packagePrograms(exampleFiles)) {
  const files = new Map([...exampleFiles].filter(([name]) => name.startsWith(program.root + '/')));
  const request = compileRequest(files, program.entry, program.wit, program.world);
  const compiled = api.compile(request.program, request.wit, request.options);
  const generated = await transpile(compiled.component.stripped, { name: 'example', instantiation: 'async', noTypescript: true, nodejsCompat: false });
  const exports = generated.exports.filter(([, type]) => type === 'function').map(([name]) => name).sort();
  if (program.export && !exports.includes(program.export)) throw new Error(`${program.id}: default function ${program.export} is not exported`);
  examples.push({ ...program, exports, files: [...files].map(([name, bytes]) => [name, name.endsWith('.wasm') ? [...bytes] : decode(bytes)]) });
  for (const [name, bytes] of files) if (name.endsWith('.json') && !name.endsWith('/package.json')) {
    const destination = join(output, 'examples', name);
    await mkdir(dirname(destination), { recursive: true });
    await writeFile(destination, bytes);
  }
}
await writeFile(join(output, 'play/examples.json'), JSON.stringify(examples));

const packages = ['@bytecodealliance/jco', '@bytecodealliance/jco-transpile', '@bytecodealliance/preview2-shim', '@bytecodealliance/preview3-shim'];
let notices = 'Perry-WIT Wasm compiler and browser runtime distribution\n\n';
notices += await readFile(join(repository, 'LICENSE'), 'utf8');
notices += '\nPerry HIR (upstream revision pinned in Cargo.toml)\n';
notices += await readFile(join(repository, 'licenses/Perry-MIT.txt'), 'utf8');
notices += '\nanyhow (version pinned in Cargo.lock)\n';
notices += await readFile(join(repository, 'licenses/anyhow-MIT.txt'), 'utf8');
for (const name of packages) {
  const directory = join(root, 'node_modules', name);
  const pkg = JSON.parse(await readFile(join(directory, 'package.json'), 'utf8'));
  notices += `${name} ${pkg.version}\n${pkg.license}\n${pkg.repository?.url || ''}\n\n`;
  for (const file of await readdir(directory)) if (/^(LICENSE|NOTICE)/i.test(file)) notices += `${file}\n${await readFile(join(directory, file), 'utf8')}\n\n`;
}
await writeFile(join(output, 'downloads/THIRD-PARTY-NOTICES.txt'), notices);
const revision = createHash('sha256');
revision.update(await readFile(compiler));
for (const name of ['style.css', 'play/app.js', 'play/workspace.js', 'play/examples.json', 'play/assets/worker.js']) revision.update(await readFile(join(output, name)));
const version = revision.digest('hex').slice(0, 12);
for (const [name, references] of [
  ['index.html', ['style.css']],
  ['play/index.html', ['../style.css', 'app.js']],
  ['play/app.js', ['./workspace.js', './examples.json', './assets/worker.js']],
]) {
  let contents = await readFile(join(output, name), 'utf8');
  for (const reference of references) contents = contents.replaceAll(reference, `${reference}?v=${version}`);
  await writeFile(join(output, name), contents);
}
const size = async names => (await Promise.all(names.map(async name => (await stat(join(output, name))).size))).reduce((a, b) => a + b, 0);
const assets = async path => (await readdir(join(output, path), { recursive: true })).filter(name => /\.(js|wasm)$/.test(name)).map(name => `${path}/${name}`);
const report = {
  htmlAndCss: await size(['index.html', 'play/index.html', 'style.css']),
  uiJavaScript: await size(['play/app.js', 'play/workspace.js']),
  compilerComponent: await size(['downloads/perry-wit.wasm']),
  browserCompiler: await size(await assets('play/assets/compiler')),
  workerJavaScript: await size(['play/assets/worker.js']),
  jcoEngines: await size(await assets('play/assets/engine')),
};
if (report.htmlAndCss > 10 * 1024) throw new Error(`HTML/CSS budget exceeded: ${report.htmlAndCss} bytes`);
await writeFile(join(output, 'sizes.json'), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(report, null, 2));
