export async function run(base: string): Promise<string> {
  const pending = fetch(base + "/gated");
  const response = await pending;
  if (response !== await pending) throw 1;
  if (response.status !== 200 || !response.ok || response.bodyUsed) throw 2;
  const release = await fetch(base + "/release");
  const releaseBody = release.text();
  const body = response.text();
  const results = await Promise.all([body, releaseBody]);
  if (!response.bodyUsed) throw 3;
  return results[0] + results[1] + await body;
}
