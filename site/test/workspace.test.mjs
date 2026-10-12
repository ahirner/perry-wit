import { test } from 'node:test';
import assert from 'node:assert/strict';
import { compileRequest, encode, packagePrograms, readFolder, worldNames } from '../play/workspace.js';

test('module identities and grouped WIT are passed as byte buffers', () => {
  const files = new Map(['src/main.ts', 'src/util.ts', 'wit/world.wit', 'wit/types.wit', 'wit/deps/math/a.wit', 'wit/deps/math/b.wit'].map(name => [name, encode(name)]));
  const request = compileRequest(files, 'src/main.ts', 'wit/world.wit', 'app');
  assert.equal(request.program.source.name, 'src/main.ts');
  assert.equal(request.program.modules[0].name, 'src/util.ts');
  assert.equal(request.wit.application.val.length, 2);
  assert.equal(request.wit.dependencies[0].val.length, 2);
  assert(request.program.source.contents instanceof Uint8Array);
});

test('folder import skips generated files and rejects excess input', async () => {
  const file = (name, contents, size = contents.length) => ({ webkitRelativePath: `project/${name}`, size, arrayBuffer: async () => encode(contents).buffer });
  const files = await readFolder([file('index.ts', 'source'), file('wit/main.wit', 'contract'), file('node_modules/huge.ts', '', 20_000_000)]);
  assert.deepEqual([...files.keys()], ['index.ts', 'wit/main.wit']);
  await assert.rejects(readFolder([file('index.ts', '', 16 * 1024 * 1024 + 1)]), /16 MiB/);
  await assert.rejects(readFolder([{ webkitRelativePath: 'project/broken.ts', size: 1, arrayBuffer: async () => new Uint8Array([255]).buffer }]), /broken.ts is not valid UTF-8/);
});

test('world picker excludes comments', () => {
  assert.deepEqual(worldNames('// world ignored {}\nworld app {} /* world other {} */'), ['app']);
});


test('package descriptions and compiler settings come from supplied manifests', () => {
  const manifest = { name: 'custom', description: 'Description supplied by the package.', main: './src/main.ts', perryWit: { wit: './wit/world.wit', world: 'app', export: 'run', functions: { run: { args: [21] } } } };
  const files = new Map([['custom/package.json', encode(JSON.stringify(manifest))], ['custom/src/main.ts', encode('source')], ['custom/wit/world.wit', encode('contract')]]);
  const [program] = packagePrograms(files);
  assert.equal(program.description, manifest.description);
  assert.equal(program.entry, 'custom/src/main.ts');
  assert.equal(program.wit, 'custom/wit/world.wit');
  assert.deepEqual(program.functions.run.args, [21]);
  manifest.description = 'Changed by the author';
  files.set('custom/package.json', encode(JSON.stringify(manifest)));
  assert.equal(packagePrograms(files)[0].description, manifest.description);
  files.delete('custom/wit/world.wit');
  assert.throws(() => packagePrograms(files), /WIT contract .* is missing/);
  manifest.main = '../outside.ts';
  files.set('custom/package.json', encode(JSON.stringify(manifest)));
  assert.throws(() => packagePrograms(files), /stay inside the package/);
});
