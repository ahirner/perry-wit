import { readFileSync } from "node:fs";
const names = ["talc", "talc_noip", "dlmalloc", "rlsf", "rlsf_full", "lol"];
const cases = [
  ["stream 8KiB ×20k", "stream_chunks", [20000, 8192], 20000],
  ["stream 64KiB ×20k", "stream_chunks", [20000, 65536], 20000],
  ["grow 4MiB doubling ×20", "grow_body", [4 << 20, 8192], 20, 20],
  ["grow 4MiB exact 8KiB ×4", "grow_exact", [4 << 20, 8192], 512, 4],
  ["churn ×200k", "churn", [200000], 200000],
  ["regions 300 tasks", "regions", [300, 48], 300],
];
const rows = [];
for (const name of names) {
  const size = readFileSync(`out/${name}-size.wasm`).length;
  const row = { name, size };
  for (const [label, fn, args, ops, reps = 1] of cases) {
    // Fresh instance per case so peak memory is per workload.
    const { instance } = await WebAssembly.instantiate(readFileSync(`out/${name}-workloads.wasm`));
    const f = instance.exports[fn];
    try {
      for (let i = 0; i < 2; i++) f(...args); // warmup
      const t = process.hrtime.bigint();
      for (let i = 0; i < reps; i++) f(...args);
      const ns = Number(process.hrtime.bigint() - t) / (ops * reps);
      row[label] = `${ns.toFixed(0)} ns / ${(instance.exports.memory.buffer.byteLength / 1048576).toFixed(1)} MiB`;
    } catch (e) { row[label] = `FAIL ${e.message}`; }
  }
  rows.push(row);
}
console.log(["allocator", "size B", ...cases.map(c => c[0])].join(" | "));
for (const r of rows) console.log([r.name, r.size, ...cases.map(c => r[c[0]])].join(" | "));
