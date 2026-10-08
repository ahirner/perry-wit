import { createHash } from "node:crypto";
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync,
} from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
if (process.argv.length !== 2 || !process.env.WASI_WIT_PATH) {
  throw new Error("Usage: nix develop -c node scripts/compare_components.mjs");
}

function command(program, args, options = {}) {
  const result = spawnSync(program, args, {
    cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, ...options,
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

const identity = sourceIdentity();
mkdirSync(join(root, "target/performance"), { recursive: true });
const output = mkdtempSync(join(root, "target/performance/run-"));
console.log(`Artifacts: ${output}`);
writeFileSync(join(output, "metadata.json"), JSON.stringify({
  source: identity,
  rustc: command("rustc", ["-Vv"]).trim(),
  cargo: command("cargo", ["-V"]).trim(),
  node: process.version,
  machine: { platform: platform(), release: release(), arch: arch(), cpu: cpus()[0]?.model },
  profile: "release",
  rustflags: process.env.RUSTFLAGS ?? "",
  harness_sha256: hash(readFileSync(join(root, "tests/component_measurement.rs"))),
  cargo_lock_sha256: hash(readFileSync(join(root, "Cargo.lock"))),
  flake_lock_sha256: hash(readFileSync(join(root, "flake.lock"))),
}, null, 2) + "\n");
writeFileSync(join(output, "source.patch"), command("git", ["diff", "HEAD", "--binary"]));
copyFileSync(fileURLToPath(import.meta.url), join(output, "compare_components.mjs"));

console.log("Building measurement test (release)…");
const build = command("cargo", [
  "test", "--release", "--locked", "-p", "perry-wit", "--test", "component_measurement",
  "--no-run", "--message-format=json",
], { stdio: ["ignore", "pipe", "inherit"] });
writeFileSync(join(output, "build.jsonl"), build);
const artifact = build.split("\n").filter(Boolean).map((line) => JSON.parse(line))
  .find((item) => item.reason === "compiler-artifact"
    && item.target.name === "component_measurement" && item.executable);
if (!artifact) throw new Error("Missing measurement executable");

const reports = {};
for (const placement of ["root-future", "spawned-task"]) {
  console.log(`Measuring ${placement}…`);
  const report = join(output, placement, "measurements.json");
  command(artifact.executable, ["--ignored", "--exact", "measure_components", "--nocapture"], {
    env: {
      ...process.env, PERRY_MEASUREMENT_OUTPUT: report, PERRY_MEASUREMENT_PLACEMENT: placement,
    },
    stdio: "inherit", timeout: 10 * 60 * 1000,
  });
  reports[placement] = JSON.parse(readFileSync(report, "utf8"));
  if (reports[placement].placement !== placement) {
    throw new Error(`Wrong executor placement in ${report}`);
  }
}
if (sourceIdentity().source_sha256 !== identity.source_sha256) {
  throw new Error(`Source changed during measurement; discard this comparison: ${output}`);
}

function median(values) {
  const ordered = [...values].sort((a, b) => a - b);
  return ordered[Math.floor(ordered.length / 2)];
}
const timingTable = [
  "| Workload | Root ms | Spawned ms | Component bytes | Guest memory bytes |",
  "| --- | ---: | ---: | ---: | ---: |",
];
for (const item of reports["root-future"].timings) {
  const spawned = reports["spawned-task"].timings.find(({ workload }) => workload === item.workload);
  if (!spawned) throw new Error(`Missing workload: ${item.workload}`);
  for (const metric of ["component_bytes", "memory_after_samples"]) {
    if (item[metric] !== spawned[metric]) {
      throw new Error(`${item.workload} ${metric} differs between executor placements`);
    }
  }
  timingTable.push(`| ${item.workload} | ${median(item.milliseconds_per_call).toFixed(3)} | ${median(spawned.milliseconds_per_call).toFixed(3)} | ${item.component_bytes} | ${item.memory_after_samples} |`);
}
const sizeTable = ["| Example | Component bytes |", "| --- | ---: |"];
for (const item of reports["root-future"].sizes) {
  const spawned = reports["spawned-task"].sizes.find(({ workload }) => workload === item.workload);
  if (item.component_bytes !== spawned?.component_bytes) {
    throw new Error(`${item.workload} size differs between executor placements`);
  }
  sizeTable.push(`| ${item.workload} | ${item.component_bytes} |`);
}
function countTable(metrics) {
  const table = [
    `| Workload / placement | ${metrics.map(([, title]) => title).join(" | ")} |`,
    `| --- | ${metrics.map(() => "---:").join(" | ")} |`,
  ];
  for (const [placement, { timings }] of Object.entries(reports)) {
    for (const { workload, counts } of timings) {
      if (!counts) continue;
      table.push(`| ${workload} / ${placement} | ${metrics.map(([metric]) => counts[metric] ?? "—").join(" | ")} |`);
    }
  }
  return table.join("\n");
}
const report = [
  "Execution time is the median of five sample means after five warmup calls on a reused instance. Both placements use current-thread Tokio. Stream/HTTP timings exclude compilation, instantiation, input preparation and assertions. HTTP responses are in memory; no network is timed.",
  timingTable.join("\n"),
  sizeTable.join("\n"),
  "Sizes are stripped components. Guest memory is committed Wasm linear memory. Stream input is 4 MiB with a 64 KiB guest limit. HTTP has a 4 MiB body and 16 MiB guest limit; bounded reads check exact-limit success and one-byte overflow. Stream/HTTP memory must not grow after warmup, and each call checks results and resource cleanup.",
  "Call counts use a separate instrumented instance and exclude warmup. Allocator entries include realloc/free and root frames; GC calls count collector entries. Raw samples and per-function counts are in measurements.json.",
  countTable([["host_polls", "Host polls"], ["stream_reads", "Read transfers"], ["worker_starts", "Worker starts"], ["callbacks", "Callbacks"]]),
  countTable([["allocations", "Allocator entries"], ["reallocations", "Realloc entries"], ["root_frames", "Root frames"], ["boxed_values", "Boxed values"], ["promises", "Promise records"], ["collections", "GC calls"], ["heap_bumps", "Heap bumps"]]),
].join("\n\n") + "\n";
writeFileSync(join(output, "comparison.md"), report);
console.log(`\n${report}\nArtifacts: ${output}`);
