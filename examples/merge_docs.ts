// 1. Initiate BOTH HTTP requests concurrently so both are in flight simultaneously!
const p1 = fetch("http://127.0.0.1:8080/doc1.json");
const p2 = fetch("http://127.0.0.1:8080/doc2.json");

// 2. Await both responses and parse JSON
const res1 = await p1;
const doc1 = await res1.json();

const res2 = await p2;
const doc2 = await res2.json();

// 3. Merge using object splatting
const merged = { ...doc1, ...doc2 };

console.log("=== MERGED DOCUMENT (SPLATTED) ===");
console.log(JSON.stringify(merged));
console.log("==================================");
