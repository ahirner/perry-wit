// Conformance Test: JSON Syntax Error Handling
// Capability: web.response_json

try {
  const res = await fetch("http://127.0.0.1:8080/invalid.json");
  await res.json();
} catch (e: any) {
  console.error("Error: JSON syntax error");
  process.exit(1);
}
