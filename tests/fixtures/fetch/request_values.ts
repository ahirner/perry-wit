function retain(value: Request): Request { return value; }
export function run(base: string): string {
  const headers = new Headers({"X-Test": "first"});
  const request = new Request(base + "/a/../resource?q=é#removed", {headers});
  headers.set("x-test", "changed");
  if (request.method !== "GET" || request.redirect !== "follow" || request.bodyUsed) return "defaults";
  if (request.url !== base + "/resource?q=%C3%A9#removed") return "url";
  if (request.headers !== request.headers || retain(request) !== request) return "identity";
  if (request.headers.get("x-test") !== "first") return "snapshot";
  const copy = new Request(request, {method: "head", redirect: "manual"});
  copy.headers.set("x-test", "copy");
  if (request.headers.get("x-test") !== "first" || copy.method !== "HEAD") return "copy";
  if (copy.redirect !== "manual" || request.bodyUsed) return "copy defaults";
  let errors = 0;
  try { new Request("invalid"); } catch { errors++; }
  try { new Request(base, {method: "trace"}); } catch { errors++; }
  try { new Request(base, {body: "get body"}); } catch { errors++; }
  try { new Request(base, {headers: {"bad name": "value"}}); } catch { errors++; }
  return errors === 4 ? "ok" : "missing constructor errors";
}
