import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const { run } = await import(pathToFileURL(process.argv[2]));
const inputs = JSON.parse(readFileSync(process.argv[3], 'utf8'));
const bytes = new DataView(new ArrayBuffer(8));
function number(value) {
  if (Number.isNaN(value)) return 'NaN';
  bytes.setFloat64(0, value, false);
  return bytes.getBigUint64(0, false).toString(16).padStart(16, '0');
}
const observations = inputs.map(input => {
  const { value, trace } = run(input);
  return { value: number(value), trace: trace.map(number) };
});
process.stdout.write(JSON.stringify(observations));
