import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const { run } = await import(pathToFileURL(process.argv[2]));
const inputs = JSON.parse(readFileSync(process.argv[3], 'utf8')).map(value =>
  value === 'NaN' ? NaN : Buffer.from(value, 'hex').readDoubleBE(0));
const bytes = new DataView(new ArrayBuffer(8));
function number(value) {
  if (typeof value !== 'number') throw new TypeError('numeric observation required');
  if (Number.isNaN(value)) return 'NaN';
  bytes.setFloat64(0, value, false);
  return bytes.getBigUint64(0, false).toString(16).padStart(16, '0');
}
const directory = mkdtempSync(join(tmpdir(), 'perry-oracle-'));
const observations = [];
try {
  writeFileSync(join(directory, 'fixture.txt'), JSON.parse(process.argv[4]));
  process.chdir(directory);
  for (const input of inputs) {
    const { value, trace } = await run(input);
    observations.push({ value: number(value), trace: trace.map(number) });
  }
} finally {
  process.chdir(tmpdir());
  rmSync(directory, { recursive: true, force: true });
}
process.stdout.write(JSON.stringify(observations));
