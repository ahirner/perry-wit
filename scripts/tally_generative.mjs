import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

if (!process.argv[2]) throw new Error('Usage: node scripts/tally_generative.mjs <goal-manifest.json>');
const manifestPath = resolve(process.argv[2]);
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const root = dirname(manifestPath);
const sources = new Set();
const campaigns = [];
const unreadable = [];
let completedPrograms = 0;
let inputExecutions = 0;
for (const entry of readdirSync(root, { withFileTypes: true })) {
  if (!entry.isDirectory() || !entry.name.startsWith('run-')) continue;
  const directory = join(root, entry.name);
  let report;
  try {
    report = JSON.parse(readFileSync(join(directory, 'report.json'), 'utf8'));
  } catch (error) {
    unreadable.push({ directory: entry.name, error: String(error) });
    continue;
  }
  if (report.replay || !(report.startedUnix >= manifest.startedUnix ||
      (manifest.legacyCampaignsThisGoal ?? []).includes(entry.name))) continue;
  if (!Number.isInteger(report.completed) || report.completed < 0 || report.completed > report.count)
    throw new Error(`Invalid completed count: ${directory}`);
  const inputs = report.inputsPerProgram ?? 12;
  for (let index = 0; index < report.completed; index++) {
    const source = readFileSync(join(directory, `case-${index}.ts`));
    sources.add(createHash('sha256').update(source).digest('hex'));
  }
  completedPrograms += report.completed;
  inputExecutions += report.completed * inputs;
  campaigns.push({
    directory: entry.name, status: report.status, completed: report.completed,
    planned: report.count, seed: report.seed, depth: report.depth,
    apiLevel: report.apiLevel ?? null, inputsPerProgram: inputs,
    revision: report.revision ?? null,
  });
}
campaigns.sort((a, b) => a.seed - b.seed || a.directory.localeCompare(b.directory));
process.stdout.write(JSON.stringify({
  generatedAt: new Date().toISOString(), startedUnix: manifest.startedUnix,
  continueUntilLocal: manifest.continueUntilLocal,
  completedPrograms, distinctSourcePrograms: sources.size, inputExecutions,
  counting: 'Completed matching programs only; replay runs excluded; distinct sources use SHA-256 of the exact TypeScript source. Running campaigns contribute only their persisted completed prefix.',
  campaigns, unreadable,
}, null, 2) + '\n');
