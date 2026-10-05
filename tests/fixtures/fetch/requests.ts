async function keep(request: Request): Promise<Request> { return request; }
export async function run(base: string): Promise<string> {
  const headers = new Headers({"X-Test": "first"});
  const bytes = new Uint8Array([65, 66, 67]);
  const original = new Request(base + "/echo", {method: "post", headers, body: bytes});
  bytes[0] = 90;
  headers.set("x-test", "changed");
  const request = new Request(original);
  if (!original.bodyUsed || request.bodyUsed || request.method !== "POST") return "body transfer";
  request.headers.append("x-test", "second");
  if (request.headers.get("x-test") !== "first, second") return "appended header";
  const held = keep(request);
  if (await held !== request || await held !== request) return "retained Request";
  const pending = fetch(request);
  if (!request.bodyUsed) return "eager body transfer";
  const response = await pending;
  let result = await response.text();
  if (original.headers.get("x-test") !== "first") return "header alias";
  let errors = 0;
  try { new Request(original); } catch { errors++; }
  const rejected = fetch(request);
  try { await rejected; } catch { errors++; }
  try { await rejected; } catch { errors++; }
  const replaced = await fetch(request, {body: "override", headers: {"X-Test": "third"}});
  result += ";";
  result += await replaced.text();
  const text = new Request(base + "/echo", {method: "POST", body: "hello"});
  const moved = new Request(text, {body: null});
  if (!text.bodyUsed || moved.headers.get("content-type") !== "text/plain;charset=UTF-8") return "text transfer";
  const last = await fetch(moved);
  result += ";";
  result += await last.text();
  const empty = new Request(base + "/echo");
  const first = await fetch(empty);
  await first.text();
  const second = await fetch(empty, {method: undefined});
  await second.text();
  if (empty.bodyUsed) return "empty body reuse";
  return result + (errors === 3 ? ";ok" : ";missing errors");
}
