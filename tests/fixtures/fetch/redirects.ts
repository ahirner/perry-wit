export async function run(base: string, other: string): Promise<string> {
  let result = "";
  const statuses: string[] = ["301", "302", "303", "307", "308"];
  for (let i = 0; i < statuses.length; i++) {
    const status = statuses[i];
    const response = await fetch(base + "/redirect/" + status, {
      method: "POST", body: "payload", headers: {Authorization: "secret", "Content-Language": "en"}
    });
    if (!response.redirected || response.url !== base + "/echo") return "redirect metadata";
    result += await response.text();
    result += ";";
  }
  const bytes = new Uint8Array([65, 66, 67]);
  const retained = fetch(base + "/redirect/307", {method: "PUT", body: bytes});
  bytes[0] = 90;
  const response = await retained;
  result += await response.text();
  result += ";";
  const across = await fetch(base + "/cross?target=" + other, {headers: {Authorization: "secret", Cookie: "private", "X-Keep": "yes"}});
  result += await across.text();
  result += ";";
  const manual = await fetch(base + "/redirect/302", {redirect: "manual"});
  if (manual.redirected || manual.status !== 302 || manual.headers.get("location") !== "/echo") return "manual metadata";
  result += await manual.text();
  result += ";";
  const missing = await fetch(base + "/missing");
  if (missing.redirected || missing.status !== 302) return "missing location";
  result += await missing.text();
  result += ";";
  let errors = 0;
  const paths: string[] = ["/loop", "/invalid"];
  for (let i = 0; i < paths.length; i++) {
    const path = paths[i];
    try { await fetch(base + path); } catch { errors++; }
  }
  try { await fetch(base + "/redirect/302", {redirect: "error"}); } catch { errors++; }
  const invalid: {redirect: string} = {redirect: "unknown"};
  try { await fetch(base + "/not-requested", invalid as RequestInit); } catch { errors++; }
  return result + (errors === 4 ? "ok" : "errors missing");
}
