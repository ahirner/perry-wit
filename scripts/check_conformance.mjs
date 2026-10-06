// Build current tests, execute them, and require all catalog evidence to pass.
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const environment = { ...process.env, CARGO: process.env.CARGO ?? 'cargo' };
const output = resolve(root, process.argv[2] ?? 'target/conformance/report.json');
if (process.argv.length > 3) throw new Error('Usage: node scripts/check_conformance.mjs [report.json]');
rmSync(output, { force: true });

function run(program, args, env = environment) {
  const result = spawnSync(program, args, { cwd: root, env, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    process.stderr.write(result.stdout ?? '');
    process.stderr.write(result.stderr ?? '');
    throw new Error(`${program} exited with ${result.status ?? result.signal}`);
  }
  return result.stdout;
}

function sourceDigest() {
  const hash = createHash('sha256');
  const paths = run('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z']).split('\0').filter(Boolean).sort();
  const relOutput = relative(root, output);
  for (const path of paths) {
    if (path === relOutput) continue;
    hash.update(path + '\0');
    try { hash.update(readFileSync(join(root, path))); }
    catch (error) { if (error.code !== 'ENOENT') throw error; hash.update('missing'); }
  }
  return hash.digest('hex');
}

const sources = sourceDigest();
console.log('Building current Cargo test executables');
const build = run('cargo', ['test', '--workspace', '--no-run', '--message-format=json']);
const executables = new Map();
for (const line of build.split('\n')) {
  if (!line.startsWith('{')) continue;
  const artifact = JSON.parse(line);
  if (artifact.reason !== 'compiler-artifact' || !artifact.profile.test || !artifact.executable) continue;
  executables.set(relative(root, artifact.target.src_path), artifact.executable);
}
const nodeTarget = 'tests/conformance_test.rs';
if (!executables.has(nodeTarget)) throw new Error('Missing Node conformance test executable');

const catalog = JSON.parse(readFileSync(join(root, 'catalog/capabilities.json'), 'utf8'));
const required = new Set(catalog.capabilities.flatMap(capability => capability.conformance).filter(id => id.startsWith('rust:')));
const discovered = new Set();
for (const [target, executable] of executables) {
  for (const line of run(executable, ['--list', '--format=terse']).split('\n')) {
    if (line.endsWith(': test')) discovered.add(`rust:${target}#${line.slice(0, -6)}`);
  }
}
for (const id of required) {
  if (!discovered.has(id)) throw new Error(`Catalog references an unknown test: ${id}`);
}

const scratch = mkdtempSync(join(tmpdir(), 'perry-evidence-'));
try {
  const evidence = [];
  let hasNonNodeFailures = false;
  for (const [target, executable] of executables) {
    if (target === nodeTarget) continue;
    console.log(`Running ${target}`);
    const result = spawnSync(executable, ['--format=pretty', '--color=never'], {
      cwd: root,
      env: environment,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    });
    if (result.error) throw result.error;
    if (result.status !== 0) {
      hasNonNodeFailures = true;
      if (result.status !== 101) {
        process.stderr.write(result.stdout ?? '');
        process.stderr.write(result.stderr ?? '');
        throw new Error(`${executable} exited with unexpected status ${result.status ?? result.signal}`);
      }
    }
    const stdout = result.stdout ?? '';
    for (const line of stdout.split('\n')) {
      const match = /^test (\S+) \.\.\. (ok|FAILED|ignored)(?:,.*)?$/.exec(line);
      if (!match) continue;
      evidence.push({
        id: `rust:${target}#${match[1]}`,
        outcome: { ok: 'passed', FAILED: 'failed', ignored: 'skipped' }[match[2]],
        details: [],
      });
    }
  }
  const path = join(scratch, 'rust.json');
  writeFileSync(path, JSON.stringify(evidence));
  mkdirSync(dirname(output), { recursive: true });
  console.log('Comparing ordinary TypeScript with Node and checking aggregate evidence');
  const nodeResult = spawnSync(executables.get(nodeTarget), [], {
    cwd: root,
    env: { ...environment, PERRY_RUST_EVIDENCE: path, PERRY_CONFORMANCE_REPORT: output },
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });

  const reportWritten = existsSync(output);
  let report = null;

  if (reportWritten) {
    report = JSON.parse(readFileSync(output, 'utf8'));
    if (sourceDigest() !== sources) {
      rmSync(output, { force: true });
      throw new Error('Source files changed during verification; rerun against a stable checkout');
    }
    report.source_sha256 = sources;
    report.compiler_revision = run('git', ['rev-parse', 'HEAD']).trim();
    writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
  }

  const failed =
    !reportWritten ||
    Boolean(report?.missing_capabilities) ||
    Boolean(report?.failing_capabilities) ||
    nodeResult.status !== 0 ||
    hasNonNodeFailures;

  if (failed) {
    if (nodeResult.stdout) process.stdout.write(nodeResult.stdout);
    if (nodeResult.stderr) process.stderr.write(nodeResult.stderr);

    const reasons = [];
    if (!reportWritten) reasons.push('conformance test did not generate a report');
    if (report?.failing_capabilities) reasons.push(`${report.failing_capabilities} failing capability contract(s)`);
    if (report?.missing_capabilities) reasons.push(`${report.missing_capabilities} missing capability contract(s)`);
    if (hasNonNodeFailures) reasons.push('Rust unit/integration test failure(s)');
    if (nodeResult.status !== 0 && !report?.failing_capabilities && !report?.missing_capabilities && !hasNonNodeFailures) {
      reasons.push(`conformance test exited with ${nodeResult.status ?? nodeResult.signal}`);
    }
    console.error(`\nConformance verification failed: ${reasons.join(', ')}`);
    if (reportWritten) console.error(`See report at: ${output}`);
    process.exit(1);
  }

  console.log(`${report.passing_capabilities}/${report.supported_capabilities} advertised capability contracts verified; ${output}`);
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
