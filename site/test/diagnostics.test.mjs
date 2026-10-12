import test from 'node:test';
import assert from 'node:assert/strict';
import { formatDiagnostics } from '../play/diagnostics.js';

test('structured diagnostics render original locations, notes and help', () => {
  assert.equal(formatDiagnostics([{code:'missing-required-field',severity:'error',message:'Missing value',span:{file:'src/é.ts',startLine:2,startColumn:3},notes:['Record fields are mandatory'],help:'Add value: null'}]), 'src/é.ts:2:3: error[missing-required-field]: Missing value\n  Record fields are mandatory\n  help: Add value: null');
  assert.equal(formatDiagnostics([{code:'parse-error',severity:'error',message:'Unexpected token'}]), 'error[parse-error]: Unexpected token');
});
