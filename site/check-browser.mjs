import assert from 'node:assert/strict';
import { readFile, mkdir, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { resolve, extname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { chromium, webkit } from 'playwright-core';

const directory = resolve(process.env.SITE_OUTPUT || 'site/dist');
const compilerVersion = JSON.parse(await readFile(join(directory, 'downloads/compiler.metadata.json'), 'utf8')).version;
const origin = 'https://playground.test';
const prefix = '/perry-wit/';
const mergedDocuments = { ...JSON.parse(await readFile('examples/merge_docs/doc1.json', 'utf8')), ...JSON.parse(await readFile('examples/merge_docs/doc2.json', 'utf8')) };
const markdownExample = { markdown: '# Field notes 🍄\n\nFound chanterelles under the birch trees.\n\nCafé stop on the way home. ☕', codePoints: 88 };
const documentPackage = JSON.parse(await readFile('examples/merge_docs/package.json', 'utf8'));
const documentUrls = documentPackage.perryWit.functions.mergeDocs.urls;
const mime = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json', '.wasm': 'application/wasm' };
const fixtures = await mkdtemp(join(tmpdir(), 'perry-folder-'));
await mkdir(join(fixtures, 'src'));
await mkdir(join(fixtures, 'wit'));
await writeFile(join(fixtures, 'src/index.ts'), "import { twice } from './math'; export function run(n: number): number { return twice(n); }");
await writeFile(join(fixtures, 'src/math.ts'), 'export function twice(n: number): number { return n * 2; }');
await mkdir(join(fixtures, 'source-only'));
await writeFile(join(fixtures, 'source-only/index.ts'), 'export function run(n: number): number { return n * 2; }');
await writeFile(join(fixtures, 'wit/world.wit'), 'package test:folder; world app { export run: func(n: f64) -> f64; } world other { export missing: func() -> u32; }');
await mkdir('site/test-results', { recursive: true });

try {
  for (const [name, engine] of [['chromium', chromium], ['webkit', webkit]]) {
    const browser = await engine.launch({ executablePath: process.env[`${name.toUpperCase()}_EXECUTABLE`] });
    try {
      const context = await browser.newContext({ viewport: { width: 1280, height: 1000 }, acceptDownloads: true, serviceWorkers: 'block' });
      const requests = [];
      const assetVersions = [];
      // Route static files directly: no development server is started or changed.
      await context.route(`${origin}/**`, async route => {
        const url = new URL(route.request().url()); requests.push(url.pathname);
        if (url.pathname.includes('/assets/')) assetVersions.push(url.searchParams.get('v'));
        if (!url.pathname.startsWith(prefix)) return route.fulfill({ status: 404, body: 'Outside repository path' });
        let path = decodeURIComponent(url.pathname.slice(prefix.length));
        if (path.endsWith('/') || !path) path += 'index.html';
        if (path.split('/').includes('..')) return route.abort();
        try { await route.fulfill({ body: await readFile(join(directory, path)), contentType: mime[extname(path)] || 'application/octet-stream' }); }
        catch { await route.fulfill({ status: 404, body: `Missing ${path}` }); }
      });
      let documentFailure = null;
      let largeResponse = false;
      const streamedText = 'Pokémon 🙂\n'.repeat(8192);
      const documentRequests = [];
      for (const [index, url] of documentUrls.entries()) {
        await context.route(url, async route => {
          documentRequests.push(url);
          await route.fulfill({
            status: documentFailure === 'http' ? 503 : 200,
            contentType: 'application/json',
            headers: { 'access-control-allow-origin': '*' },
            body: documentFailure === 'json' && index === 0 ? 'invalid JSON' : largeResponse && index === 1 ? JSON.stringify({ streamedText }) : await readFile(`examples/merge_docs/doc${index + 1}.json`),
          });
        });
      }
      const page = await context.newPage();
      page.on('pageerror', error => console.error(name, error));
      await page.goto(`${origin}${prefix}`);
      assert(!requests.some(path => /\.wasm$|\/assets\/|\.js$/.test(path)), 'Homepage must load no scripts or compiler');
      assert.equal(await page.locator('.build > li').count(), 3);
      assert.equal(await page.locator('.build details').count(), 0);
      assert.equal(await page.locator('.build pre').innerText(), 'nix build github:ahirner/perry-wit#perry-wit');
      assert.match(await page.locator('.build li').last().innerText(), /Wasm compiler.*the compiler itself/);
      assert.equal(await page.locator('a[href$="perry-wit-interfaces.tar.gz"]').count(), 0);
      for (const href of await page.locator('.build a').evaluateAll(links => links.map(link => link.getAttribute('href')))) {
        assert((await readFile(join(directory, href))).length > 0, `Missing resource: ${href}`);
      }
      assert.equal(await page.locator('footer a[href*="downloads/"], footer a[href*="rust/"]').count(), 0);
      assert.equal(await page.locator('footer span').innerText(), `Perry-WIT v${compilerVersion}`);
      const footerText = await page.locator('footer').innerText();
      const footerLinks = await page.locator('footer a').evaluateAll(links => links.map(link => link.href));
      await page.screenshot({ path: `site/test-results/${name}-home.png`, fullPage: true });
      const codeStyle = await page.locator('.editors pre').first().evaluate(element => { const style = getComputedStyle(element); return [style.backgroundColor, style.border, style.borderRadius, style.padding, style.font]; });
      await page.getByRole('link', { name: 'Try the playground' }).click();
      assert.deepEqual(await page.locator('#source').evaluate(element => { const style = getComputedStyle(element); return [style.backgroundColor, style.border, style.borderRadius, style.padding, style.font]; }), codeStyle, 'Homepage and playground code fields share the same styling');
      await page.waitForFunction(() => document.getElementById('source').value.length > 0);
      assert.equal(await page.locator('footer').innerText(), footerText);
      assert.deepEqual(await page.locator('footer a').evaluateAll(links => links.map(link => link.href)), footerLinks);
      for (const id of ['source-file-picker', 'wit-file-picker', 'entry-picker']) assert.equal(await page.locator(`#${id}`).isVisible(), false);
      assert.deepEqual(await page.locator('#function option').evaluateAll(options => options.map(option => option.value)), ['countCodePoints', 'renderMarkdown']);
      assert.equal(await page.locator('#world-picker').isVisible(), false);
      assert.match(await page.locator('#world-description').textContent(), /merge-task/);
      assert(!await page.locator('#wit').inputValue().then(value => value.includes('world merge-docs')));
      assert(!requests.some(path => /\.wasm$|\/assets\//.test(path)), 'Compiler must load on first compilation');
      const run = async () => {
        await page.locator('#run').click();
        await page.waitForFunction(() => ['complete', 'error'].includes(document.getElementById('status').dataset.state), undefined, { timeout: 40_000 });
        const output = await page.locator('#output').textContent();
        assert.equal(await page.locator('#status').getAttribute('data-state'), 'complete', output);
        return output;
      };
      await page.locator('#run').scrollIntoViewIfNeeded();
      const beforeRun = await page.locator('#run').evaluate(element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, scroll: scrollY }; });
      const shifts = [];
      const tracking = setInterval(async () => { try { shifts.push(await page.locator('#run').evaluate(element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, scroll: scrollY }; })); } catch {} }, 30);
      try { assert.deepEqual(JSON.parse(await run()), markdownExample); }
      finally { clearInterval(tracking); }
      shifts.push(await page.locator('#run').evaluate(element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, scroll: scrollY }; }));
      for (const position of shifts) assert.deepEqual(position, beforeRun, 'Running must not move controls or scroll the page');
      assert(assetVersions.length > 1 && assetVersions.every(version => version && version === assetVersions[0]), 'Worker, compiler and engine assets must share a build version');
      await page.screenshot({ path: `site/test-results/${name}-example.png`, fullPage: true });
      await page.locator('#args').fill('{bad json');
      await page.locator('#run').click();
      assert.match(await page.locator('#status').textContent(), /arguments/);
      assert.equal(await page.locator('#args').getAttribute('aria-invalid'), 'true');
      await page.locator('#reset').click();
      assert.equal(await page.locator('#function').inputValue(), 'renderMarkdown');
      const contract = await page.locator('#wit').inputValue();
      await page.locator('#wit').fill(contract + '\nworld other { export missing: func() -> u32; }');
      await page.locator('#world').selectOption('other');
      assert.equal(await page.locator('#function').isDisabled(), true, 'Changing the contract invalidates the previous callable choices');
      await page.locator('#run').click();
      await page.waitForFunction(() => document.getElementById('status').dataset.state === 'error');
      assert.match(await page.locator('#output').textContent(), /World “other” requires an exported missing function/);
      assert.equal(await page.locator('#output').getAttribute('data-state'), 'error');
      await page.screenshot({ path: `site/test-results/${name}-error.png`, fullPage: true });
      await page.locator('#reset').click();
      assert.equal(await page.locator('#world-picker').isVisible(), false);
      assert.equal(await page.locator('#world').inputValue(), 'merge-task');
      await run();
      await page.locator('#tab-interface').click();
      assert.match(await page.locator('#output').textContent(), /render-markdown/);
      await page.locator('#tab-wat').click();
      assert.match(await page.locator('#output').textContent(), /\(component/);
      for (const extension of ['wasm', 'wat']) {
        const download = page.waitForEvent('download');
        await page.locator(`#download-${extension}`).click();
        assert.equal((await download).suggestedFilename(), `component.${extension}`);
      }
      await page.locator('#tab-result').click();
      await page.locator('#args').fill('["Edited 🙂", ["  é  ", "  ", "👩‍💻"]]');
      assert.deepEqual(JSON.parse(await run()), { markdown: '# Edited 🙂\n\né\n\n👩‍💻', codePoints: 19 });
      await page.locator('#function').selectOption('countCodePoints');
      assert.equal(typeof JSON.parse(await page.locator('#args').inputValue())[0], 'string', 'Switching example functions supplies matching arguments');
      await page.locator('#args').fill('["é👩‍💻"]');
      assert.equal(JSON.parse(await run()), 5);
      await page.locator('#source').press('End'); await page.keyboard.type(' ');
      assert.equal(await page.locator('#function').isDisabled(), false, 'Editing keeps the function choices');
      await page.locator('#example').selectOption('merge_docs');
      assert.deepEqual(JSON.parse(await page.locator('#args').inputValue()), documentUrls);
      assert(documentUrls.every(url => url.startsWith('https://pokeapi.co/api/v2/')));
      assert.deepEqual(JSON.parse(await run()), mergedDocuments);
      assert.deepEqual(documentRequests.sort(), [...documentUrls].sort());
      largeResponse = true;
      assert.equal(JSON.parse(await run()).streamedText, streamedText, 'Large Unicode response bodies must cross the stream adapter without a timeout');
      largeResponse = false;
      for (const [failure, message] of [['http', /503/], ['json', /JSON/]]) {
        documentFailure = failure;
        await page.locator('#run').click();
        await page.waitForFunction(() => document.getElementById('status').dataset.state === 'error');
        assert.match(await page.locator('#output').textContent(), message);
        assert.equal(await page.locator('#download-wasm').isDisabled(), false);
      }
      documentFailure = null;
      assert.deepEqual(JSON.parse(await run()), mergedDocuments);
      await page.locator('#source').fill('export function {');
      await page.locator('#compile').click();
      await page.waitForFunction(() => document.getElementById('status').dataset.state === 'error');
      assert.match(await page.locator('#output').textContent(), /Parsing|Parse error/);
      await page.locator('#source').fill('export function run(): number { return new Missing(); }');
      await page.locator('#wit').fill('package test:diagnostics; world app { export run: func() -> f64; }');
      await page.locator('#compile').click();
      await page.waitForFunction(() => document.getElementById('status').dataset.state === 'error');
      await page.locator('#tab-log').click();
      assert.match(await page.locator('#output').textContent(), /Warning: unknown identifier 'Missing'/, 'Upstream compiler stderr reaches diagnostics through WASI');
      await page.locator('#tab-result').click();
      await page.locator('#reset').click();
      await page.locator('#folder-input').setInputFiles(resolve('examples'));
      await page.waitForFunction(() => document.getElementById('status').textContent === 'Folder loaded');
      assert.equal(await page.locator('#entry').inputValue(), 'merge_task/index.ts');
      const taskPackage = JSON.parse(await readFile('examples/merge_task/package.json', 'utf8'));
      assert.equal(await page.locator('#folder-description').textContent(), taskPackage.description);
      assert.equal(await page.locator('#function').isDisabled(), true, 'Imported callable choices must be verified by compilation');
      assert.deepEqual(JSON.parse(await run()), markdownExample);
      await page.locator('#entry').selectOption('merge_docs/index.ts');
      assert.equal(await page.locator('#world').inputValue(), 'merge-docs');
      assert.deepEqual(JSON.parse(await run()), mergedDocuments);
      await page.locator('#folder-input').setInputFiles(resolve('examples/merge_task'));
      await page.waitForFunction(() => document.getElementById('status').textContent === 'Folder loaded');
      assert.equal(await page.locator('#entry-picker').isVisible(), false);
      assert.deepEqual(JSON.parse(await run()), markdownExample);
      await page.locator('#function').selectOption('countCodePoints');
      assert.equal(JSON.parse(await run()), 6);
      await page.locator('#reset').click();
      assert.equal(await page.locator('#function').isDisabled(), true);
      assert.match(await page.locator('#args-help').textContent(), /A title and an array/);
      assert.deepEqual(JSON.parse(await run()), markdownExample, 'Reset restores matching arguments and the default function for an imported package');
      await page.locator('#folder-input').setInputFiles(join(fixtures, 'source-only'));
      await page.waitForFunction(() => document.getElementById('status').textContent === 'Folder loaded');
      assert.equal(await page.locator('#wit').inputValue(), '');
      assert.match(await page.locator('#folder-description').textContent(), /No WIT contract was included/);
      await page.locator('#wit').fill('package test:source; world app { export run: func(n: f64) -> f64; }');
      await page.locator('#args').fill('[21]');
      assert.equal(await run(), '42');
      await page.locator('#folder-input').setInputFiles(fixtures);
      await page.waitForFunction(() => document.getElementById('status').textContent === 'Folder loaded');
      assert.equal(await page.locator('#source-file-picker').isVisible(), true);
      assert.equal(await page.locator('#entry-picker').isVisible(), true);
      assert.equal(await page.locator('#wit-file-picker').isVisible(), false);
      assert.equal(await page.locator('#function').isDisabled(), true);
      assert.equal(await page.locator('#world-picker').isVisible(), true);
      assert.equal(await page.locator('#world').inputValue(), '');
      await page.locator('#entry').selectOption('source-only/index.ts');
      assert.equal(await page.locator('#source-file').inputValue(), 'source-only/index.ts', 'Changing the entrypoint shows its source');
      await page.locator('#entry').selectOption('src/index.ts');
      await page.locator('#run').click();
      assert.match(await page.locator('#output').textContent(), /Select a WIT world/);
      await page.locator('#world').selectOption('app');
      await page.locator('#args').fill('[21]');
      assert.equal(await run(), '42');
      await page.locator('#source-file').selectOption('src/math.ts');
      await page.locator('#source').fill('export function twice(n: number): number { return n * 3; }');
      assert.equal(await run(), '63');
      await page.locator('#reset').click(); assert.equal(await run(), '42');
      await page.locator('#source-file').selectOption('src/index.ts');
      await page.locator('#source').fill("import { readFile } from 'node:fs/promises'; export async function run(): Promise<string> { return await readFile('input.txt', 'utf8'); }");
      await page.locator('#wit').fill('package test:fs; world app { include wasi:cli/imports@0.3.0; export run: async func() -> string; }');
      await page.locator('#args').fill('[]');
      await page.locator('#run').click();
      await page.waitForFunction(() => document.getElementById('status').dataset.state === 'error');
      assert.match(await page.locator('#output').textContent(), /cannot provide import wasi:filesystem/);
      assert.equal(await page.locator('#download-wasm').isDisabled(), false);
      await page.locator('#source').fill('export async function run(n: number): Promise<number> { console.log("executed", n); return n * 2; }');
      await page.locator('#wit').fill('package test:logging; world app { include wasi:cli/imports@0.3.0; export run: async func(n: f64) -> f64; }');
      await page.locator('#args').fill('[21]');
      assert.equal(await run(), '42');
      await page.locator('#tab-log').click();
      assert.match(await page.locator('#output').textContent(), /executed 21/);
      await page.locator('#tab-result').click();
      await page.locator('#reset').click(); await page.locator('#args').fill('[21]');
      await page.clock.install();
      await page.locator('#run').click(); await page.clock.fastForward(31_000);
      assert.equal(await page.locator('#status').getAttribute('data-state'), 'error');
      assert.match(await page.locator('#output').textContent(), /exceeded 30 seconds/);
      await page.clock.resume();
      assert.equal(await run(), '42');
      await page.locator('#source-file').selectOption('src/index.ts');
      await page.locator('#source').fill('export function run(n: number): number { while (true) {} return n; }');
      await page.locator('#run').click(); await page.locator('#cancel').click();
      assert.equal(await page.locator('#status').textContent(), 'Cancelled');
      await page.locator('#reset').click(); assert.equal(await run(), '42');
      await page.locator('#source').fill('export function mathTwice(n: number): number { return n * 2; }');
      await page.locator('#wit').fill('package test:math; interface math { twice: func(n: f64) -> f64; } world app { export math; }');
      assert.equal(await run(), '42');
      assert.match(await page.locator('#function').inputValue(), /#twice$/);
      await page.locator('#reset').click(); assert.equal(await run(), '42');
      await page.locator('#close-folder').click();
      assert(await page.locator('#wit').evaluate(element => element.scrollLeft === 0 && element.scrollTop === 0), 'Switching workspaces restores the editor view');
      await page.locator('#args').focus();
      await page.keyboard.press('ControlOrMeta+Enter');
      await page.waitForFunction(() => document.getElementById('status').dataset.state === 'complete');
      await page.screenshot({ path: `site/test-results/${name}-desktop.png`, fullPage: true });
      await page.setViewportSize({ width: 800, height: 1000 });
      assert(await page.locator('#status').evaluate(element => element.clientWidth >= element.scrollWidth), 'The split layout must leave room to read the run status');
      await page.setViewportSize({ width: 390, height: 844 });
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Mobile layout overflows');
      assert(await page.locator('#example').evaluate(element => element.clientWidth >= 190 && element.offsetHeight >= 40), 'Mobile example choices must be readable and easy to tap');
      await page.locator('#run').scrollIntoViewIfNeeded();
      const mobilePosition = await page.locator('#run').evaluate(element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, scroll: scrollY }; });
      await run();
      assert.deepEqual(await page.locator('#run').evaluate(element => { const box = element.getBoundingClientRect(); return { x: box.x, y: box.y, scroll: scrollY }; }), mobilePosition, 'Mobile run controls must stay in place');
      await page.screenshot({ path: `site/test-results/${name}-mobile.png`, fullPage: true });
      await page.emulateMedia({ colorScheme: 'dark' });
      await page.screenshot({ path: `site/test-results/${name}-dark.png`, fullPage: true });
      await page.goto(`${origin}${prefix}`);
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Mobile homepage overflows');
      await page.screenshot({ path: `site/test-results/${name}-home-mobile.png`, fullPage: true });
      console.log(`${name}: examples, world choices, actionable errors, argument presets, reset, keyboard, folder imports, cancellation, downloads, mobile/dark layouts and repository path passed`);
    } finally { await browser.close(); }
  }
} finally { await rm(fixtures, { recursive: true, force: true }); }
