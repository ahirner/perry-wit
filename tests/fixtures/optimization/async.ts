async function increment(n: number): Promise<number> {
  return n + 1;
}

export async function run(n: number): Promise<number> {
  const values = [n, n * 2];
  const next = await increment(n);
  return values[0] + values[1] + next;
}
