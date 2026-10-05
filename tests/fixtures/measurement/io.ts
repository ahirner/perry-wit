import { readFile, writeFile } from 'fs/promises';

export async function runTask(path: string): Promise<string> {
  const data = (await readFile(path, 'utf8'));
  (await writeFile(path + '.copy', data, 'utf8'));
  return data;
}
