// Conformance Test: performance.now and Monotonic Clocks
// Capability: web.clocks

const p1 = performance.now();
const p2 = performance.now();

console.log(p2 >= p1 ? "MONOTONIC_OK" : "MONOTONIC_FAIL");
console.log(p1 > 0 ? "POSITIVE_OK" : "POSITIVE_FAIL");
