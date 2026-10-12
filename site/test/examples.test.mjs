import { test } from 'node:test';
import assert from 'node:assert/strict';
import { countCodePoints, renderMarkdown } from '../../examples/merge_task/index.ts';

test('Markdown counts match JavaScript string iteration across Unicode sequences', () => {
  for (const [text, expected] of [['', 0], ['Café 🙂', 6], ['e\u0301', 2], ['👩‍💻', 3]]) {
    assert.equal(countCodePoints(text), expected);
  }
  assert.deepEqual(renderMarkdown('  Notes 🙂  ', [' e\u0301 ', ' ', '👩‍💻']), {
    markdown: '# Notes 🙂\n\ne\u0301\n\n👩‍💻', codePoints: 18,
  });
  assert.deepEqual(renderMarkdown(' ', ['', ' ']), { markdown: '', codePoints: 0 });
});
