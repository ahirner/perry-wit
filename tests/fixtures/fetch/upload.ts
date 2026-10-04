export async function run(base: string): Promise<string> {
    const options = {method: "post", headers: {"x-request": " \toriginal \t "}, body: "héllo🙂"};
    const first = fetch(base + "/text", options);
    options.headers["x-request"] = "changed";
    const text = await (await first).text();
    const bytes = new Uint8Array(70001);
    bytes[0] = 65;
    bytes[70000] = 66;
    const second = fetch(base + "/bytes", {method: "PATCH", headers: {"Content-Type": "application/octet-stream"}, body: bytes});
    bytes[0] = 90;
    bytes[70000] = 90;
    const uploaded = await (await second).text();
    return text + "|" + uploaded;
}
