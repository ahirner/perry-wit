// Conformance Test: Fetch Failure & Error Exit
// Capability: web.fetch, node.process_exit

try {
  const res = await fetch("http://127.0.0.1:19999/down");
  await res.json();
} catch (e: any) {
  console.error("Error: connection refused");
  process.exit(1);
}
