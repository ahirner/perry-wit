// 1. Initiate BOTH HTTP requests concurrently using Promise.all
const [res1, res2] = await Promise.all([
    fetch("http://127.0.0.1:8080/doc1.json"),
    fetch("http://127.0.0.1:8080/doc2.json")
]);
// 2. Parse JSON documents
const doc1 = await res1.json();
const doc2 = await res2.json();

// 3. Merge using object splatting
const merged = { ...doc1, ...doc2 };

console.log("=== MERGED DOCUMENT (SPLATTED) ===");
console.log(JSON.stringify(merged));
console.log("==================================");
