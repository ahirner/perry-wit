import { load, save } from 'test:record-flow/storage';
import type { Entry, Failure } from 'test:record-flow/storage';
type Selected = { ok: true; value: Entry[] } | { ok: false; error: Failure };
type Saved = { ok: true; value: number } | { ok: false; error: Failure };

export function select(entries: Entry[], prefix: string): Selected {
  if (entries.length > 64) { return { ok: false, error: 'too-many' }; }
  const output: Entry[] = [];
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    if (entry.key === '' || entry.title === '') { return { ok: false, error: 'invalid' }; }
    if (entry.active === false) { continue; }
    const updated: Entry = { key: entry.key, title: prefix + entry.title, active: true };
    let replaced = false;
    for (let j = 0; j < output.length; j++) {
      if (output[j].key === entry.key) { output[j] = updated; replaced = true; break; }
    }
    if (!replaced) { output.push(updated); }
  }
  return { ok: true, value: output };
}

export async function run(prefix: string): Promise<Saved> {
  const loaded = await load();
  if (!loaded.ok) { return { ok: false, error: loaded.error }; }
  const selected = select(loaded.value, prefix);
  if (!selected.ok) { return { ok: false, error: selected.error }; }
  return await save(selected.value);
}
