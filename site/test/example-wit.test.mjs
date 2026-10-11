import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { exampleWit } from '../example-wit.mjs';
import { worldNames } from '../play/workspace.js';

test('bundled examples expose only their own world and required interfaces', async () => {
  const source = await readFile(new URL('../../wit/world.wit', import.meta.url), 'utf8');
  const task = exampleWit(source, 'merge-task');
  assert.deepEqual(worldNames(task), ['merge-task']);
  assert.match(task, /record document/);
  assert.match(task, /export render-markdown/);
  assert.match(task, /export count-code-points/);
  assert(!task.includes('include task-runner'));
  const docs = exampleWit(source, 'merge-docs');
  assert.deepEqual(worldNames(docs), ['merge-docs']);
  assert.match(docs, /import wasi:http\/client@0.3.0/);
  assert.match(docs, /record merge-error/);
  assert(!docs.includes('document-types'));
  assert.equal(await readFile(new URL('../../examples/merge_task/world.wit', import.meta.url), 'utf8'), task);
  assert.equal(await readFile(new URL('../../examples/merge_docs/world.wit', import.meta.url), 'utf8'), docs);
});

test('example package extraction fails on missing or cyclic worlds', () => {
  assert.throws(() => exampleWit('package test:example; world a {}', 'b'), /Invalid example world/);
  assert.throws(() => exampleWit('package test:example; world a { include b; } world b { include a; }', 'a'), /Invalid example world/);
});
