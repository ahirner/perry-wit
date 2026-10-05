function retain(response: Response): Response { return response; }
interface Init { status?: number; statusText?: string; headers?: Headers; }
function initialized(init: Init): Response { return new Response(null, init); }
export function metadata(): string {
  const headers = new Headers({"X-Test": "first"});
  const response = new Response("hello", {status: 201, statusText: "Créé\tOK", headers});
  headers.set("x-test", "external");
  if (response.status !== 201 || !response.ok || response.statusText !== "Créé\tOK") return "status";
  if (response.url !== "" || response.redirected || response.bodyUsed) return "metadata";
  if (retain(response) !== response || response.headers !== response.headers) return "identity";
  if (response.headers.get("x-test") !== "first") return "header snapshot";
  response.headers.set("x-test", "changed");
  if (response.headers.get("x-test") !== "changed" || headers.get("x-test") !== "external") return "mutable headers";
  if (response.headers.get("content-type") !== "text/plain;charset=UTF-8") return "default content type";
  const typed = new Response("{}", {headers: {"Content-Type": "application/json"}});
  if (typed.headers.get("content-type") !== "application/json") return "explicit content type";
  const binary = new Response(new Uint8Array([1]));
  if (binary.headers.has("content-type")) return "binary content type";
  const pairs: [string, string][] = [["X-Test", "pair"]];
  const paired = new Response(null, {headers: pairs, status: 404});
  if (paired.ok || paired.headers.get("x-test") !== "pair") return "header pairs or error status";
  if (initialized({}).status !== 200 || initialized({status: undefined}).status !== 200) return "optional status";
  if (initialized({status: 200.9}).status !== 200 || initialized({status: 65737}).status !== 201) return "numeric conversion";
  if (new Response().statusText !== "" || new Response(null).status !== 200) return "defaults";
  let errors = 0;
  try { new Response(null, {status: 199}); } catch { errors++; }
  try { new Response(null, {status: 600}); } catch { errors++; }
  try { new Response(null, {status: 0 / 0}); } catch { errors++; }
  try { new Response(null, {status: 1 / 0}); } catch { errors++; }
  try { initialized({status: 0 / 0}); } catch { errors++; }
  try { new Response("", {status: 204}); } catch { errors++; }
  try { new Response(new Uint8Array(0), {status: 205}); } catch { errors++; }
  try { new Response("x", {status: 304}); } catch { errors++; }
  try { new Response(null, {statusText: "bad\rtext"}); } catch { errors++; }
  try { new Response(null, {statusText: "Ā"}); } catch { errors++; }
  try { new Response(null, {headers: {"bad name": "x"}}); } catch { errors++; }
  return errors === 11 ? "ok" : "missing constructor errors";
}
async function retained(response: Response): Promise<Response> { return response; }
export async function run(): Promise<string> {
  const bytes = new Uint8Array([239, 187, 191, 97, 255, 98]);
  const response = new Response(bytes);
  bytes[3] = 122;
  const identity = retained(response);
  if (await identity !== response || await identity !== response) return "promise identity";
  const pending = response.text();
  if (!response.bodyUsed || await pending !== "a�b" || await pending !== "a�b") return "consumption";
  let errors = 0;
  try { await response.text(); } catch { errors++; }
  const json = new Response('{"label":"漢🙂"}', {headers: {"content-type": "application/json"}});
  const parsed = await json.json();
  if (typeof parsed !== "object" || typeof parsed.label !== "string" || parsed.label !== "漢🙂") return "JSON";
  const invalid = new Response("{bad");
  try { await invalid.json(); } catch { errors++; }
  if (!invalid.bodyUsed) return "JSON used";
  const binary = new Response(new Uint8Array([1, 2, 3]));
  const bufferTask = binary.arrayBuffer();
  const buffer = await bufferTask;
  if (buffer !== await bufferTask || new Uint8Array(buffer)[2] !== 3) return "buffer identity";
  const utf8 = new Response("é");
  const output = await utf8.bytes();
  if (output.length !== 2 || output[0] !== 195 || output[1] !== 169) return "UTF-8";
  const empty = new Response(null, {status: 204});
  if (await empty.text() !== "" || await empty.text() !== "" || empty.bodyUsed) return "null consumption";
  const emptyText = new Response("");
  if (await emptyText.text() !== "" || !emptyText.bodyUsed) return "empty text";
  try { await emptyText.text(); } catch { errors++; }
  return errors === 3 ? "ok" : "missing body errors";
}
