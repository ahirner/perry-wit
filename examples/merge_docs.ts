export async function mergeDocs(): Promise<string> {
  const [res1, res2] = await Promise.all([
    fetch("http://127.0.0.1:8080/doc1.json"),
    fetch("http://127.0.0.1:8080/doc2.json")
  ]);
  const doc1 = await res1.json();
  const doc2 = await res2.json();
  return JSON.stringify({ ...doc1, ...doc2 });
}
