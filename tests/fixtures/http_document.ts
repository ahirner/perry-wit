import { get } from "perry:http";

// The runner http-fetch document contract, with resolved request coordinates.
export async function run(authority: string, path: string): Promise<Result<string, number>> {
    const response = await get("http", authority, path, { accept: "application/json, text/plain, application/xml, text/xml" }, 4 * 1024 * 1024);
    if (response.status < 200 || response.status >= 300) { throw 400; }
    const decoder = new TextDecoder();
    let count = 0;
    let contentType = "";
    for (let index = 0; index < response.headerCount; index++) {
        if (response.headerName(index) === "content-type") {
            count++;
            contentType = decoder.decode(response.headerValue(index));
        }
    }
    if (count !== 1) { throw 401; }
    let end = contentType.indexOf(";");
    if (end < 0) { end = contentType.length; }
    let start = 0;
    while (start < end && (contentType.charAt(start) === " " || contentType.charAt(start) === "\t")) { start++; }
    while (end > start && (contentType.charAt(end - 1) === " " || contentType.charAt(end - 1) === "\t")) { end--; }
    const media = contentType.slice(start, end).toLowerCase();
    if (media !== "application/json" && media !== "text/plain" && media !== "application/xml" && media !== "text/xml") { throw 402; }
    const text = decoder.decode(response.body);
    if (media === "application/json") { JSON.parse(text); }
    return text;
}
