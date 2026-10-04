import { readFileSync, writeFileSync } from 'fs';

export function runTask(path: string): string {
  const data = readFileSync(path, 'utf8');
  writeFileSync(path + '.copy', data, 'utf8');
  return data;
}
