export function run(): number {
  const controller = new AbortController();
  const signal: AbortSignal = controller.signal;
  if (signal !== controller.signal || signal.aborted) throw 1;
  const first = new Request("http://example.test/", { signal });
  const second = new Request(first);
  const independent = new Request(first, { signal: null });
  if (first.signal === signal || first.signal === second.signal) throw 2;
  for (let i = 0; i < 2000; i++) {
    const discarded = new AbortController();
    discarded.abort();
  }
  controller.abort();
  controller.abort();
  if (!signal.aborted || !first.signal.aborted || !second.signal.aborted) throw 3;
  if (independent.signal.aborted) throw 4;
  return 42;
}
