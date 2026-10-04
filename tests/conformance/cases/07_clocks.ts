export function runRun(): {ok:true}|{ok:false} {
  // Conformance Test: performance.now and Monotonic Clocks
  // Capability: web.clocks

  const p1 = performance.now();
  const p2 = performance.now();

  if (p2 >= p1) console.log("MONOTONIC_OK"); else console.log("MONOTONIC_FAIL");
  if (p1 > 0) console.log("POSITIVE_OK"); else console.log("POSITIVE_FAIL");
  return {ok:true};
}
