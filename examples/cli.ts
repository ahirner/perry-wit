import { setTimeout } from "node:timers/promises";

console.log("Perry CLI: starting");
const start = performance.now();
await setTimeout(1);
if (performance.now() < start) throw new Error("monotonic clock moved backwards");
console.log("Perry CLI: finished");
