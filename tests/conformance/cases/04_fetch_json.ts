// Conformance Test: Fetch JSON Response
// Capability: web.fetch, web.response_json

const res = await fetch("http://127.0.0.1:8080/doc1.json");
const doc = await res.json();

console.log(JSON.stringify(doc));
