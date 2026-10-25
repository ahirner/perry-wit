// Handbook generation and checks: every claim the build can execute is executed here, so a
// compiler change that invalidates a page fails the site build instead of shipping stale prose.
import { readFile, writeFile, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { createOnigurumaEngine } from '@shikijs/engine-oniguruma';
import { codeToTokensWithThemes, createShikiPrimitiveAsync } from '@shikijs/primitive';
import { compileRequest, encode } from './play/workspace.js';

// Languages highlighted at build time, by fence or `data-language` name. Other blocks stay plain.
const LANGUAGES = { ts: 'typescript', typescript: 'typescript', wit: 'wit', wat: 'wasm', sh: 'shellscript', rust: 'rust' };
const THEMES = { light: 'github-light-high-contrast', dark: 'github-dark-high-contrast' };

/** Write generated pages into `bookDir/src` and check snippets, listings and tables. */
export async function prepareBook({ repository, bookDir, generated, api, conformanceDoc }) {
  const pages = [join(repository, 'README.md')];
  for (const name of await readdir(join(repository, 'docs/src'))) if (name.endsWith('.md')) pages.push(join(repository, 'docs/src', name));
  const groups = new Map();
  const sources = new Map();
  for (const page of pages) {
    const text = await readFile(page, 'utf8');
    sources.set(page, text);
    for (const [, lang, id, body] of text.matchAll(/^```(wit|ts) check=([\w-]+)\n([\s\S]*?)^```$/gm)) {
      if (!groups.has(id)) groups.set(id, { page, files: {} });
      const group = groups.get(id);
      if (group.page !== page || group.files[lang]) throw new Error(`${page}: check=${id} needs one ts block and at most one wit block on one page`);
      group.files[lang] = await expandIncludes(body, dirname(page));
    }
  }
  const compiled = new Map();
  for (const [id, { page, files }] of groups) {
    if (!files.ts) throw new Error(`${page}: check=${id} needs a ts block`);
    compiled.set(id, compile(api, `${page}#${id}`, files.ts, files.wit));
  }

  // A page including `ID-wat.md` gets the core module of check group ID, and every instruction
  // it names in a code span must occur in that module.
  for (const [page, text] of sources) {
    const listings = [...text.matchAll(/\{\{#include ([\w-]+)-wat\.md\}\}/g)].map(match => match[1]);
    if (listings.length === 0) continue;
    let printed = '';
    for (const id of listings) {
      const group = groups.get(id);
      if (!group) throw new Error(`${page}: no check=${id} snippets for ${id}-wat.md`);
      const core = compile(api, `${page}#${id}`, group.files.ts, group.files.wit, true).core;
      const wat = execFileSync('wasm-tools', ['print', '-'], { input: Buffer.from(core), encoding: 'utf8' }).trim();
      await writeFile(join(bookDir, 'src', `${id}-wat.md`), `\`\`\`wat\n${wat}\n\`\`\`\n`);
      printed += wat;
    }
    for (const [, instruction] of text.matchAll(/`([if](?:32|64)\.[a-z0-9_]+)`/g)) {
      if (!printed.includes(instruction)) throw new Error(`${page} names ${instruction}, which the generated listing lacks:\n${printed}`);
    }
  }

  // A table after `<!-- invoke: ID -->` lists calls into check group ID and their printed results.
  for (const [page, text] of sources) {
    for (const [, id, table] of text.matchAll(/<!-- invoke: ([\w-]+) -->\n((?:\|.*\n)+)/g)) {
      const component = compiled.get(id)?.component?.stripped;
      if (!component) throw new Error(`${page}: invoke table needs check=${id} snippets`);
      const file = join(generated, `${id}.wasm`);
      await writeFile(file, component);
      for (const [, call, expected] of table.matchAll(/^\| `([^`]+)` \| (`[^`]*`|trap) \|/gm)) {
        const outcome = invoke(file, call);
        const wanted = expected === 'trap' ? 'trap' : expected.slice(1, -1);
        if (outcome !== wanted) throw new Error(`${page}: ${call} printed ${outcome}, the table says ${wanted}`);
      }
    }
  }

  await writeFile(join(generated, 'ARCHITECTURE.md'), await readFile(join(repository, 'ARCHITECTURE.md'), 'utf8'));
  await writeFile(join(bookDir, 'src/conformance-catalog.md'), conformanceDoc);
  await writeFile(join(bookDir, 'src/stdlib-inventory.md'), await inventory(repository, api));
  await writeFile(join(bookDir, 'src/embedding-example.rs'), doctest(await readFile(join(repository, 'src/api.rs'), 'utf8'), 'compile'));
}

/** Compile the homepage's TypeScript and WIT example and run its export once with Wasmtime. */
export async function checkHomepage({ html, generated, api }) {
  const section = html.match(/<section class="editors"[\s\S]*?<\/section>/)?.[0] ?? '';
  const [ts, wit] = [...section.matchAll(/<pre><code>([\s\S]*?)<\/code><\/pre>/g)].map(match => decodeHtml(match[1]));
  if (!ts || !wit) throw new Error('site/index.html needs a TypeScript and a WIT block in .editors');
  const component = compile(api, 'site/index.html', ts, wit).component.stripped;
  const file = join(generated, 'homepage.wasm');
  await writeFile(file, component);
  const greeting = invoke(file, 'greet("World")');
  if (greeting !== '"Hello, World"') throw new Error(`site/index.html example printed ${greeting}`);
}

/**
 * Highlight code to `<code>` contents. Text in a theme's default color stays bare, and every other
 * light and dark color pair becomes a class in `palette`, which `paletteStyle` defines.
 */
export async function codeHighlighter() {
  const load = async (kind, name) => (await import(`@shikijs/${kind}/${name}`)).default;
  const primitive = await createShikiPrimitiveAsync({
    themes: await Promise.all(Object.values(THEMES).map(name => load('themes', name))),
    langs: await Promise.all([...new Set(Object.values(LANGUAGES))].map(name => load('langs', name))),
    engine: createOnigurumaEngine(import('@shikijs/engine-oniguruma/wasm-inlined')),
  });
  const defaults = Object.values(THEMES).map(name => primitive.getTheme(name).fg.toLowerCase());
  return (text, language, palette) => codeToTokensWithThemes(primitive, text, { lang: LANGUAGES[language], themes: THEMES })
    .map(line => {
      const runs = [];
      for (const { content, variants } of line) {
        const colors = Object.keys(THEMES).map((theme, index) => (variants[theme].color ?? defaults[index]).toLowerCase());
        // Whitespace shows no color, so it extends the current run.
        const pair = /^\s*$/.test(content) ? runs.at(-1)?.pair ?? null
          : colors.every((color, index) => color === defaults[index]) ? null : colors.join(' ');
        if (pair && !palette.has(pair)) palette.set(pair, `t${palette.size}`);
        if (runs.at(-1)?.pair === pair) runs.at(-1).text += content;
        else runs.push({ pair, text: content });
      }
      return runs.map(({ pair, text }) => pair ? `<span class="${palette.get(pair)}">${encodeHtml(text)}</span>` : encodeHtml(text)).join('');
    })
    .join('\n');
}

/** A `<style>` element that gives each palette class its light and dark color. */
function paletteStyle(palette) {
  const rules = [...palette].map(([pair, name]) => {
    const [light, dark] = pair.split(' ');
    return `.${name}{--shiki-light:${light};--shiki-dark:${dark}}`;
  });
  return `<style>${rules.join('')}</style>\n`;
}

/** Highlight the homepage's `.editors` blocks by their `data-language`. */
export function highlightHomepage(html, highlight) {
  const palette = new Map();
  const highlighted = html.replace(/(<div data-language="(\w+)">[\s\S]*?<pre><code>)([\s\S]*?)(?=<\/code>)/g,
    (_, head, language, code) => head + highlight(decodeHtml(code), language, palette));
  return highlighted.replace('</head>', paletteStyle(palette) + '</head>');
}

/** Highlight the code blocks of every handbook page, marked so mdBook's highlight.js skips them. */
export async function highlightBook(directory, highlight) {
  for (const name of await readdir(directory)) {
    if (!name.endsWith('.html')) continue;
    const file = join(directory, name);
    const palette = new Map();
    const html = (await readFile(file, 'utf8')).replace(/<pre(?: class="playground")?><code class="language-(\w+)[^"]*">([\s\S]*?)<\/code><\/pre>/g, (block, language, code) => {
      if (!LANGUAGES[language]) return block;
      // Hidden lines are the `fn main` that mdBook wraps around Rust examples.
      const text = decodeHtml(code.replace(/<span class="boring">[^<]*<\/span>/g, '')).replace(/\n$/, '');
      return `<pre><code class="nohighlight">${highlight(text, language, palette)}</code></pre>`;
    });
    await writeFile(file, palette.size ? html.replace('</head>', paletteStyle(palette) + '</head>') : html);
  }
}

function encodeHtml(text) {
  return text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

function decodeHtml(html) {
  const entities = { lt: '<', gt: '>', quot: '"', '#39': "'", amp: '&' };
  return html.replace(/&(lt|gt|quot|#39|amp);/g, (_, name) => entities[name]);
}

/** Replace `{{#include path}}` lines with the file they name, relative to the page. */
async function expandIncludes(body, base) {
  const lines = [];
  for (const line of body.split('\n')) {
    const include = line.match(/^\{\{#include (.+)\}\}$/);
    lines.push(include ? (await readFile(join(base, include[1]), 'utf8')).trimEnd() : line);
  }
  return lines.join('\n');
}

/** Compile one snippet pair through the compiler component, naming the snippet on failure. */
function compile(api, label, ts, wit, coreOnly = false) {
  const program = { source: { name: 'src/index.ts', contents: encode(ts) }, modules: [] };
  try {
    const files = wit ? [{ name: 'world.wit', contents: encode(wit) }] : api.generateWit(program);
    const contract = { application: { tag: 'sources', val: files }, dependencies: [] };
    return api.compile(program, contract, { coreOnly });
  } catch (error) {
    throw new Error(`${label} failed to compile: ${error.message || JSON.stringify(error.payload || error)}`);
  }
}

/** Run one `wasmtime --invoke` call and return its printed result, or `trap`. */
function invoke(file, call) {
  try {
    return execFileSync('wasmtime', ['run', '-C', 'cache=n', '-S', 'p3=y', '-W', 'component-model-async=y', '--invoke', call, file], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  } catch (error) {
    if (/wasm trap/.test(error.stderr ?? '')) return 'trap';
    throw new Error(`wasmtime --invoke '${call}' failed: ${error.stderr || error.message}`);
  }
}

/** The body of the doctest on `pub fn name` in a Rust source, without hidden lines. */
function doctest(source, name) {
  const before = source.slice(0, source.indexOf(`\npub fn ${name}(`)).split('\n');
  const docs = [];
  for (let i = before.length - 1; i >= 0 && before[i].startsWith('///'); i--) docs.unshift(before[i].replace(/^\/\/\/ ?/, ''));
  const fence = docs.join('\n').match(/```\n([\s\S]*?)```/);
  if (!fence) throw new Error(`src/api.rs: pub fn ${name} has no doctest`);
  return fence[1].split('\n').filter(line => line !== '#' && !line.startsWith('# ')).join('\n').trim() + '\n';
}

// Globals the inventory lists. Each statement must compile; the build compiles them all at once.
const GLOBALS = [
  ['Number', 'Number.isFinite', 'Number.isFinite(1)'],
  ['Number', 'Number.isInteger', 'Number.isInteger(1)'],
  ['Number', 'Number.isNaN', 'Number.isNaN(1)'],
  ['Number', 'Number.isSafeInteger', 'Number.isSafeInteger(1)'],
  ['Number', 'Number.parseInt', 'Number.parseInt("1")'],
  ['Number', 'Number.parseFloat', 'Number.parseFloat("1.5")'],
  ['Number', 'parseInt', 'parseInt("1")'],
  ['Number', 'parseFloat', 'parseFloat("1.5")'],
  ['Number', 'isNaN', 'isNaN(1)'],
  ['Number', 'Number()', 'Number("1")'],
  ['String', 'String()', 'String(1)'],
  ['String', 'String.fromCodePoint', 'String.fromCodePoint(65)'],
  ['Array', 'Array.isArray', 'Array.isArray([1])'],
  ['Array', 'Array.from', 'Array.from([1, 2])'],
  ['Array', 'Array.of', 'Array.of(1, 2)'],
  ['Object', 'Object.keys', 'Object.keys({ a: 1 })'],
  ['Object', 'Object.values', 'Object.values({ a: 1 })'],
  ['Object', 'Object.entries', 'Object.entries({ a: 1 })'],
  ['Object', 'Object.is', 'Object.is(1, 1)'],
  ['Object', 'structuredClone', 'structuredClone({ a: 1 })'],
  ['JSON', 'JSON.parse', 'JSON.parse("[1]") as number[]'],
  ['JSON', 'JSON.stringify', 'JSON.stringify({ a: 1 })'],
  ['Errors', 'Error', 'new Error("x")'],
  ['Errors', 'TypeError', 'new TypeError("x")'],
  ['Errors', 'RangeError', 'new RangeError("x")'],
  ['Errors', 'ReferenceError', 'new ReferenceError("x")'],
  ['Errors', 'SyntaxError', 'new SyntaxError("x")'],
  ['Promise', 'Promise.resolve', 'Promise.resolve(1)'],
  ['Promise', 'Promise.all', 'await Promise.all([Promise.resolve(1)])'],
  ['Promise', 'Promise.allSettled', 'await Promise.allSettled([Promise.resolve(1)])'],
  ['Promise', 'Promise.race', 'await Promise.race([Promise.resolve(1)])'],
  ['Time', 'Date', 'new Date(0)'],
  ['Time', 'Date.now', 'Date.now()'],
  ['Time', 'Date.parse', 'Date.parse("2024-01-01")'],
  ['Time', 'Date.UTC', 'Date.UTC(2024, 0, 1)'],
  ['Time', 'performance.now', 'performance.now()'],
  ['Time', 'Temporal.Now.instant', 'Temporal.Now.instant()'],
  ['Time', 'Temporal.Instant.from', 'Temporal.Instant.from("2024-01-01T00:00:00Z")'],
  ['Time', 'Temporal.Instant.fromEpochMilliseconds', 'Temporal.Instant.fromEpochMilliseconds(0)'],
  ['Time', 'Temporal.PlainDate.from', 'Temporal.PlainDate.from("2024-02-29")'],
  ['Time', 'Temporal.PlainDate.compare', 'Temporal.PlainDate.compare(Temporal.PlainDate.from("2024-02-29"), Temporal.PlainDate.from("2024-03-01"))'],
  ['Time', 'Temporal.PlainDateTime.from', 'Temporal.PlainDateTime.from("2024-02-29T10:00")'],
  ['Binary, text', 'Uint8Array', 'new Uint8Array(4)'],
  ['Binary, text', 'TextEncoder', 'new TextEncoder()'],
  ['Binary, text', 'TextDecoder', 'new TextDecoder().decode(new Uint8Array(1))'],
  ['Collections', 'Map', 'new Map<string, number>()'],
  ['Collections', 'Set', 'new Set<number>()'],
  ['Web', 'URL', 'new URL("https://example.test/a")'],
  ['Web', 'AbortController', 'new AbortController()'],
  ['Web', 'crypto.getRandomValues', 'crypto.getRandomValues(new Uint8Array(4))'],
  ['Web', 'crypto.randomUUID', 'crypto.randomUUID()'],
  ['Web', 'fetch', 'await fetch("https://example.test/")'],
  ['Web', 'Headers', 'new Headers()'],
  ['Web', 'Request', 'new Request("https://example.test/")'],
  ['Web', 'Response', 'new Response("x")'],
  ['Process', 'console.log', 'console.log("x")'],
  ['Process', 'console.info', 'console.info("x")'],
  ['Process', 'console.warn', 'console.warn("x")'],
  ['Process', 'console.error', 'console.error("x")'],
  ['Process', 'console.debug', 'console.debug("x")'],
  ['Process', 'process.env', 'process.env.HOME'],
  ['Process', 'process.exitCode', 'process.exitCode = 0'],
  ['Process', 'process.exit', 'process.exit(0)'],
];
const PROBE_WIT = 'package docs:probe;\n\nworld probe {\n  include wasi:cli/command@0.3.0;\n  import wasi:http/client@0.3.0;\n}\n';

// Display names for the receiver kinds in the frontend method registry.
const RECEIVERS = new Map([
  ['PlainDateTime', 'Temporal.PlainDateTime'], ['PlainDate', 'Temporal.PlainDate'], ['Instant', 'Temporal.Instant'],
  ['SearchParams', 'URLSearchParams'], ['ByobReader', 'ReadableStreamBYOBReader'],
  ['Response', 'Response'], ['Request', 'Request'],
  ['Writable', 'WritableStream'], ['Readable', 'ReadableStream'], ['Headers', 'Headers'], ['Decoder', 'TextDecoder'],
  ['Number', 'number'], ['String', 'string'], ['Array', 'Array'], ['Bytes', 'Uint8Array'], ['Stats', 'fs.Stats'],
  ['Regex', 'RegExp'], ['Abort', 'AbortController'], ['Writer', 'WritableStreamDefaultWriter'],
  ['Reader', 'ReadableStreamDefaultReader'], ['ReadResult', 'ReadableStream read result'], ['Date', 'Date'], ['Map', 'Map'],
  ['Set', 'Set'], ['Url', 'URL'],
]);

/** The inventory: probed globals, then methods and module functions read from compiler tables. */
async function inventory(repository, api) {
  const lines = GLOBALS.map(([, , statement], index) => /^(console\.|process\.)/.test(statement) ? `${statement};` : `const probe${index} = ${statement};`);
  try {
    compile(api, 'inventory probe', lines.join('\n') + '\n', PROBE_WIT);
  } catch {
    const failing = GLOBALS.filter((_, index) => { try { compile(api, 'probe', lines[index] + '\n', PROBE_WIT); return false; } catch { return true; } });
    throw new Error(`Inventory globals no longer compile: ${failing.map(([, label]) => label).join(', ')}`);
  }
  const globals = new Map();
  for (const [group, label] of GLOBALS) globals.set(group, [...(globals.get(group) ?? []), label]);

  const lower = join(repository, 'crates/perry-lower/src');
  const registry = (await readFile(join(repository, 'crates/perry-frontend/src/methods/registry.rs'), 'utf8')).split('\n');
  const methods = new Map();
  let receivers = [];
  for (const line of registry) {
    const group = line.match(/^\s*(Receiver::.*),\s*"[^"]+"\s*\{$/);
    if (group) {
      receivers = [...group[1].matchAll(/Receiver::(Map|Set)\b|Kind::(\w+)/g)].map(([, collection, kind]) => {
        const name = RECEIVERS.get(collection || kind);
        if (!name) throw new Error(`Unknown method registry receiver: ${collection || kind}`);
        return name;
      });
      continue;
    }
    const entry = line.match(/^\s*([A-Z][A-Za-z]+?)\s+("[^"]+"(?:\s*\|\s*"[^"]+")*)\s+/);
    // An entry whose lowering recipe rejects the call documents a rejection, not support.
    if (!entry || /=>\s*reject\(/.test(line)) continue;
    if (receivers.length === 0) throw new Error(`No method registry receiver for ${entry[1]}`);
    const names = [...entry[2].matchAll(/"([^"]+)"/g)].map(match => match[1]).filter(name => !name.startsWith('$$'));
    for (const receiver of receivers) methods.set(receiver, new Set([...(methods.get(receiver) ?? []), ...names]));
  }
  const expressions = await readFile(join(lower, 'expressions.rs'), 'utf8');
  const program = await readFile(join(lower, 'program.rs'), 'utf8');
  const math = [...new Set([...expressions.matchAll(/Expr::Math([A-Za-z]+)\b/g)].map(match => match[1].replace(/Spread$/, '').toLowerCase()))];
  const modules = [
    ['`Math`', math],
    ['`node:fs/promises`', [...program.matchAll(/\(BuiltinModule::Fs,\s*"([^"]+)"\)/g)].map(match => match[1])],
    ['`node:timers/promises`', [...program.matchAll(/\(BuiltinModule::Timers,\s*"([^"]+)"\)/g)].map(match => match[1])],
    ['`node:stream`', [...program.matchAll(/\(BuiltinModule::Stream,\s*"([^"]+)"\)/g)].map(match => `${match[1]}.toWeb`)],
  ];
  const code = names => [...names].map(name => `\`${name}\``).join(', ');
  return [
    '| Globals | |', '| :--- | :--- |', ...[...globals].map(([group, labels]) => `| ${group} | ${code(labels)} |`), '',
    '| Modules | Functions |', '| :--- | :--- |', ...modules.map(([name, functions]) => `| ${name} | ${code(functions)} |`), '',
    '| Receiver | Members |', '| :--- | :--- |', ...[...methods].map(([receiver, names]) => `| ${receiver} | ${code(names)} |`), '',
  ].join('\n');
}
