import { get } from "perry:http";

export async function run(authority: string, path: string): Promise<Result<string, number>> {
  const response = await get("http", authority, path, { accept: "application/json" }, 65536);
  if (response.status !== 200) { throw 400; }
  let found = false;
  const decoder = new TextDecoder();
  for (let index = 0; index < response.headerCount; index++) {
    if (response.headerName(index) === "content-type") {
      if (found) { throw 401; }
      found = true;
      if (decoder.decode(response.headerValue(index)) !== "application/json") { throw 402; }
    }
  }
  if (!found) { throw 401; }
  const text = decoder.decode(response.body);
  JSON.parse(text);
  return text;
}
