export async function mergeDocs(firstUrl: string, secondUrl: string): Promise<string> {
  const [first, second] = await Promise.all([
    fetch(firstUrl),
    fetch(secondUrl),
  ]);
  if (!first.ok || !second.ok) throw new Error("HTTP " + first.status + " / " + second.status);
  const [base, update] = await Promise.all([first.json(), second.json()]);
  return JSON.stringify({ ...base, ...update });
}
