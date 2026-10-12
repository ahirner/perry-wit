import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { worldNames } from '../play/workspace.js';

test('bundled examples expose only their own world and required interfaces', async () => {
  const task = await readFile(new URL('../../examples/merge_task/world.wit', import.meta.url), 'utf8');
  assert.deepEqual(worldNames(task), ['merge-task']);
  assert.match(task, /record document/);
  assert.match(task, /export render-markdown/);
  assert.match(task, /export count-code-points/);
  const docs = await readFile(new URL('../../examples/merge_docs/world.wit', import.meta.url), 'utf8');
  assert.deepEqual(worldNames(docs), ['merge-docs']);
  assert.match(docs, /import wasi:http\/client@0.3.0/);
  assert.match(docs, /record merge-error/);
  assert(!docs.includes('document-types'));
});
