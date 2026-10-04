export async function run(url: string): Promise<string> {
    const headers = new Headers({"X-Request":"one"});
    headers.append("x-request", "two");
    const options = {method:"POST", headers:headers, body:"body"};
    const first = fetch(url, options);
    headers.set("x-request", "changed");
    const response = await first;
    let guarded = 0;
    try { response.headers.set("x-request", "bad"); } catch { guarded++; }
    try { response.headers.append("x-request", "bad"); } catch { guarded++; }
    try { response.headers.delete("x-request"); } catch { guarded++; }
    const text = await response.text();
    if(guarded !== 3) return "immutable response";
    if(text !== "one, two:body") return "request snapshot:"+text;
    const copied = new Headers(response.headers);
    copied.set("x-request", "copy");
    copied.delete("content-length");
    const second = await fetch(url, {method:"POST",headers:copied,body:"again"});
    const secondText = await second.text();
    const pairs: [string, string][] = [["x-request","pairs"],["X-Request","last"]];
    const third = await fetch(url, {headers:pairs});
    return secondText+"|"+(await third.text());
}
