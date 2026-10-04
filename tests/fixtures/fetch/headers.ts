async function retain(headers: Headers): Promise<Headers> { await 0; return headers; }

export async function run(url: string): Promise<string> {
    const response = await fetch(url);
    const headers: Headers = response.headers;
    const pending = retain(headers);
    const value = headers.get("x-VaLuE");
    const body = await response.text();
    if ((await pending) !== headers) return "retained header identity";
    if (headers !== response.headers) return "header identity";
    if (!headers.has("X-VALUE") || headers.has("missing")) return "presence";
    if (headers.get("missing") !== null) return "missing";
    if (value === null) return "value shape";
    const headerBytes = headers.get("x-bytes");
    if (headerBytes === null) return "missing header bytes";
    if (headerBytes !== "Ã©") return "header bytes";
    if (headers.get("x-empty") !== "") return "empty";
    if (headers.get("x-separate") !== "left, right") return "combining";
    for (let i = 0; i < 2000; i++) { const garbage = url.toLowerCase().split(".").join("-"); }
    if (headers.get("x-value") !== value) return "released metadata";
    let failed = false;
    try { headers.get("invalid name"); } catch (error) { failed = true; }
    if (!failed) return "invalid name accepted";
    return value + ":" + body;
}
