import { createHash } from "node:crypto";
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync,
} from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const baselineRef = process.argv[2] ?? "main";
if (process.argv.length > 3 || !process.env.WASI_P3_WIT_PATH) {
  throw new Error("Usage: nix develop -c node scripts/compare_components.mjs [baseline-ref]");
}

function command(program, args, cwd = root, options = {}) {
  const result = spawnSync(program, args, {
    cwd, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, ...options,
  });
  if (result.error || result.status !== 0) {
    throw new Error(`${program} ${args.join(" ")} failed: ${result.error || result.stderr || result.stdout || result.status}`);
  }
  return result.stdout;
}

const hash = (data) => createHash("sha256").update(data).digest("hex");
function sourceIdentity() {
  const paths = command("git", ["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
    .split("\0").filter(Boolean).sort();
  const digest = createHash("sha256");
  for (const path of paths) {
    digest.update(path).update("\0");
    digest.update(existsSync(join(root, path)) ? readFileSync(join(root, path)) : "<deleted>");
    digest.update("\0");
  }
  return {
    commit: command("git", ["rev-parse", "HEAD"]).trim(),
    status: command("git", ["status", "--short"]),
    source_sha256: digest.digest("hex"),
  };
}

const baselineCommit = command("git", ["rev-parse", "--verify", `${baselineRef}^{commit}`]).trim();
const currentIdentity = sourceIdentity();
mkdirSync(join(root, "target/conformance/comparisons"), { recursive: true });
const output = mkdtempSync(join(root, "target/conformance/comparisons/run-"));
console.log(`Artifacts: ${output}`);
const baselineSource = join(output, "baseline-source");
mkdirSync(baselineSource);
command("git", ["archive", "--format=tar", `--output=${join(output, "baseline.tar")}`, baselineCommit]);
command("tar", ["-xf", join(output, "baseline.tar"), "-C", baselineSource]);

// Both revisions use exactly the same harness. Each keeps its own compiler and examples.
for (const path of ["tests/component_measurement.rs", "tests/fixtures/bounded_response.ts", "tests/fixtures/bounded_byob_response.ts"]) {
  mkdirSync(dirname(join(baselineSource, path)), { recursive: true });
  copyFileSync(join(root, path), join(baselineSource, path));
}

const metadata = {
  baseline: { ref: baselineRef, commit: baselineCommit },
  current: currentIdentity,
  rustc: command("rustc", ["-Vv"]).trim(),
  cargo: command("cargo", ["-V"]).trim(),
  node: process.version,
  machine: { platform: platform(), release: release(), arch: arch(), cpu: cpus()[0]?.model },
  profile: "release",
  rustflags: process.env.RUSTFLAGS ?? "",
  harness_sha256: hash(readFileSync(join(root, "tests/component_measurement.rs"))),
  flake_lock_sha256: hash(readFileSync(join(root, "flake.lock"))),
  method: "Median of five sample means on a reused instance after five warmup calls. Stream/HTTP: five calls per sample. Builds, compilation, instantiation, input preparation and outcome assertions are outside the stream/HTTP timers. HTTP uses an in-memory host response; no network is timed.",
};
writeFileSync(join(output, "current.patch"), command("git", ["diff", "HEAD", "--binary"]));
copyFileSync(fileURLToPath(import.meta.url), join(output, "compare_components.mjs"));
writeFileSync(join(output, "metadata.json"), JSON.stringify(metadata, null, 2) + "\n");

const revisions = [
  { name: "main", source: baselineSource },
  { name: "current", source: root },
];
for (const revision of revisions) {
  revision.output = join(output, revision.name);
  mkdirSync(revision.output);
  copyFileSync(join(revision.source, "Cargo.lock"), join(revision.output, "Cargo.lock"));
  console.log(`Building ${revision.name} measurement test (release)…`);
  const build = command("cargo", [
    "test", "--release", "--locked", "-p", "perry-wit", "--test", "component_measurement",
    "--no-run", "--message-format=json",
  ], revision.source, {
    env: { ...process.env, CARGO_TARGET_DIR: join(root, "target") },
    stdio: ["ignore", "pipe", "inherit"],
  });
  writeFileSync(join(revision.output, "build.jsonl"), build);
  const artifact = build.split("\n").filter(Boolean).map((line) => JSON.parse(line))
    .find((item) => item.reason === "compiler-artifact"
      && item.target.name === "component_measurement" && item.executable);
  if (!artifact) throw new Error(`Missing ${revision.name} measurement executable`);
  revision.executable = join(revision.output, "measurement-test");
  copyFileSync(artifact.executable, revision.executable);
}

// Finish both builds before timing either revision to avoid compiler CPU contention.
for (const revision of revisions) {
  console.log(`Measuring ${revision.name}…`);
  const report = join(revision.output, "measurements.json");
  command(revision.executable, ["--ignored", "--exact", "measure_components", "--nocapture"], revision.source, {
    env: { ...process.env, PERRY_MEASUREMENT_OUTPUT: report },
    stdio: "inherit", timeout: 10 * 60 * 1000,
  });
  revision.report = JSON.parse(readFileSync(report, "utf8"));
}
if (sourceIdentity().source_sha256 !== currentIdentity.source_sha256) {
  throw new Error(`Source changed during measurement; discard this comparison: ${output}`);
}

function median(values) {
  const ordered = [...values].sort((a, b) => a - b);
  return ordered[Math.floor(ordered.length / 2)];
}
function delta(before, after) {
  const percent = 100 * (after / before - 1);
  return `${percent >= 0 ? "+" : ""}${percent.toFixed(1)}%`;
}
const rows = ["incoming-stream", "bounded-http-exact", "bounded-http-overflow",
  "incoming-stream-byob", "bounded-http-exact-byob", "bounded-http-overflow-byob",
  "merge_docs.ts", "merge_task.ts", "template-task"];
const table = [
  "| Workload | Baseline ms/call | Current ms/call | Time change | Baseline bytes | Current bytes | Size change |",
  "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
];
for (const workload of rows) {
  const [before, after] = revisions.map(({ report }, index) => {
    const name = index === 0 ? workload.replace(/-byob$/, "") : workload;
    return [...report.timings, ...report.sizes].find((item) => item.workload === name);
  });
  if (!before || !after) throw new Error(`Missing workload: ${workload}`);
  const timed = before.milliseconds_per_call !== undefined;
  const times = timed
    ? [median(before.milliseconds_per_call), median(after.milliseconds_per_call)] : [];
  table.push(`| ${workload} | ${timed ? times[0].toFixed(3) : "—"} | ${timed ? times[1].toFixed(3) : "—"} | ${timed ? delta(...times) : "—"} | ${before.component_bytes} | ${after.component_bytes} | ${delta(before.component_bytes, after.component_bytes)} |`);
}
const report = [
  `Baseline: \`${baselineRef}\` at \`${baselineCommit}\`.`,
  `Current: \`${currentIdentity.commit}\`${currentIdentity.status ? " (working tree changes; see metadata.json and current.patch)" : ""}.`,
  metadata.method,
  table.join("\n"),
  "All sizes are stripped component bytes. Both HTTP rows use the same component and a 4 MiB response; limits are 4 MiB and 4 MiB − 1 byte. Incoming stream sums 4 MiB under a 64 KiB guest memory limit. HTTP has a 16 MiB guest limit. Neither grows after warmup; every call checks its result and resource cleanup.",
  "BYOB rows compare caller-provided buffers with the same workload using the original API on the baseline. Original default-reader rows remain separate. Both HTTP implementations enforce the same cap and probe for overflow.",
  "Examples use each revision’s own source. Timings are local measurements, not a significance test; inspect raw samples and repeat when differences are small. The existing text/filesystem measurements are retained in the JSON reports.",
].join("\n\n") + "\n";
writeFileSync(join(output, "comparison.md"), report);
console.log(`\n${report}\nArtifacts: ${output}`);
