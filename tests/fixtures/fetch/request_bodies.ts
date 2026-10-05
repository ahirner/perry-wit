export async function run(): Promise<string> {
  const bytes = new Uint8Array([239, 187, 191, 97, 255, 98]);
  const text = new Request("https://example.com", {method: "POST", body: bytes});
  bytes[3] = 122;
  const pending = text.text();
  if (!text.bodyUsed) return "eager consumption";
  if (await pending !== "a�b" || await pending !== "a�b") return "decoding or observation";
  let errors = 0;
  try { await text.bytes(); } catch { errors++; }
  const json = new Request("https://example.com", {method: "POST", body: '{"label":"漢🙂"}'});
  const parsed = await json.json();
  if (typeof parsed !== "object") return "JSON shape";
  if (typeof parsed.label !== "string" || parsed.label !== "漢🙂") return "JSON value";
  const invalid = new Request("https://example.com", {method: "POST", body: "{bad"});
  try { await invalid.json(); } catch { errors++; }
  if (!invalid.bodyUsed) return "JSON failure consumption";
  const binary = new Request("https://example.com", {method: "POST", body: new Uint8Array([1, 2, 3])});
  const bufferTask = binary.arrayBuffer();
  const buffer = await bufferTask;
  if (buffer !== await bufferTask) return "buffer identity";
  const view = new Uint8Array(buffer);
  if (view[2] !== 3 || view.length !== 3) return "buffer bytes";
  const bytesRequest = new Request("https://example.com", {method: "POST", body: "é"});
  const output = await bytesRequest.bytes();
  if (output.length !== 2 || output[0] !== 195 || output[1] !== 169) return "UTF-8 bytes";
  const empty = new Request("https://example.com");
  if (await empty.text() !== "" || await empty.text() !== "") return "null body text";
  const emptyBytes = await empty.bytes();
  if (emptyBytes.length !== 0 || empty.bodyUsed) return "null body bytes";
  try { await empty.json(); } catch { errors++; }
  if (empty.bodyUsed) return "null JSON consumption";
  const emptyText = new Request("https://example.com", {method: "POST", body: ""});
  if (await emptyText.text() !== "" || !emptyText.bodyUsed) return "empty text body";
  try { await emptyText.text(); } catch { errors++; }
  return errors === 4 ? "ok" : "missing errors";
}
