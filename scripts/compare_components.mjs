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
  method: "Median of five sample means on a reused instance after five warmup calls. Both revisions run in both placements on a current-thread Tokio runtime: directly awaited by its root future, and in an ordinary spawned task. Stream/HTTP: five calls per sample. Builds, compilation, instantiation, input preparation and outcome assertions are outside the stream/HTTP timers. HTTP uses an in-memory host response; no network is timed.",
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
const placements = ["root-future", "spawned-task"];
for (const placement of placements) {
  for (const revision of revisions) {
    console.log(`Measuring ${revision.name} (${placement})…`);
    const report = join(revision.output, placement, "measurements.json");
    command(revision.executable, ["--ignored", "--exact", "measure_components", "--nocapture"], revision.source, {
      env: {
        ...process.env, PERRY_MEASUREMENT_OUTPUT: report, PERRY_MEASUREMENT_PLACEMENT: placement,
      },
      stdio: "inherit", timeout: 10 * 60 * 1000,
    });
    revision.reports ??= {};
    revision.reports[placement] = JSON.parse(readFileSync(report, "utf8"));
    if (revision.reports[placement].placement !== placement) {
      throw new Error(`Wrong executor placement in ${report}`);
    }
  }
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
const rows = ["incoming-stream", "incoming-stream-byob", "bounded-http-exact",
  "bounded-http-overflow", "bounded-http-exact-byob", "bounded-http-overflow-byob",
  "unbounded-http-array-buffer"];
function comparison(workload, placement) {
  return revisions.map(({ reports }, index) => {
    const name = index !== 0 ? workload
      : workload === "unbounded-http-array-buffer" ? "bounded-http-exact"
        : workload.replace(/-byob$/, "");
    const report = reports[placement];
    const result = [...report.timings, ...report.sizes].find((item) => item.workload === name);
    if (!result) throw new Error(`Missing workload: ${name} (${placement})`);
    return result;
  });
}
const timingTable = [
  "| Workload | Baseline root ms | Current root ms | Change | Baseline spawned ms | Current spawned ms | Change |",
  "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
];
for (const workload of rows) {
  const values = placements.flatMap((placement) => {
    const times = comparison(workload, placement).map((item) => median(item.milliseconds_per_call));
    return [...times.map((time) => time.toFixed(3)), delta(...times)];
  });
  timingTable.push(`| ${workload} | ${values.join(" | ")} |`);
}
const sizeTable = [
  "| Workload | Baseline component bytes | Current component bytes | Change | Baseline guest memory bytes | Current guest memory bytes |",
  "| --- | ---: | ---: | ---: | ---: | ---: |",
];
for (const workload of [...rows, "merge_docs.ts", "merge_task.ts", "template-task"]) {
  const [before, after] = comparison(workload, "root-future");
  const spawned = comparison(workload, "spawned-task");
  for (const [index, item] of [before, after].entries()) {
    for (const metric of ["component_bytes", "memory_after_samples"]) {
      if (item[metric] !== spawned[index][metric]) {
        throw new Error(`${workload} ${metric} differs between executor placements`);
      }
    }
  }
  sizeTable.push(`| ${workload} | ${before.component_bytes} | ${after.component_bytes} | ${delta(before.component_bytes, after.component_bytes)} | ${before.memory_after_samples ?? "—"} | ${after.memory_after_samples ?? "—"} |`);
}
const report = [
  `Baseline: \`${baselineRef}\` at \`${baselineCommit}\`.`,
  `Current: \`${currentIdentity.commit}\`${currentIdentity.status ? " (working tree changes; see metadata.json and current.patch)" : ""}.`,
  metadata.method,
  timingTable.join("\n"),
  sizeTable.join("\n"),
  "All sizes are stripped component bytes. Guest memory is committed Wasm linear memory after warmup, not process RSS. Exact and overflow HTTP rows share a component and a 4 MiB response; limits are 4 MiB and 4 MiB − 1 byte. Incoming stream sums 4 MiB under a 64 KiB guest memory limit. HTTP has a 16 MiB guest limit. Neither grows after warmup; every call checks its result and resource cleanup.",
  "BYOB rows compare caller-provided buffers with the same workload using the original API on the baseline. Original default-reader rows remain separate. Both HTTP implementations enforce the same cap and probe for overflow.",
  "The unbounded row uses new Uint8Array(await response.arrayBuffer()), with no guest body-size cap and no caller-provided buffer. It is one guest-level await, not necessarily one internal read. Its baseline reference is the original perry:http exact-limit read of the same 4 MiB body: the baseline retains its cap, so these rows have different overflow guarantees. The harness memory limit still applies.",
  "Examples use each revision’s own source. Timings are local measurements, not a significance test; inspect raw samples and repeat when differences are small. The existing text/filesystem measurements are retained in the JSON reports.",
].join("\n\n") + "\n";
writeFileSync(join(output, "comparison.md"), report);
console.log(`\n${report}\nArtifacts: ${output}`);
