function select(headers: Headers, label: string): string { headers.append("chosen",label); return label; }
function decorate(headers: Headers): Headers {
    headers.append("X-Mixed", " last ");
    return headers;
}
export function run(): string {
    const source: [string, string][] = [["X-Mixed", "first"], ["x-mixed", "second"], ["Empty", ""]];
    const pairs = new Headers(source);
    source[0] = ["X-Mixed", "changed"];
    if (pairs.get("X-MIXED") !== "first, second") return "pair snapshot";
    const copy = new Headers(pairs);
    if (decorate(copy) !== copy) return "identity";
    if (copy.get("x-mixed") !== "first, second, last") return "append";
    copy.set("X-Mixed", "\té\t");
    if (copy.get("x-mixed") !== "é") return "set";
    if (pairs.get("x-mixed") !== "first, second") return "copy isolation";
    copy.delete("x-MIXED");
    if (copy.has("x-mixed") || copy.get("x-mixed") !== null) return "delete";
    copy.delete("missing");
    if (copy.get("empty") !== "") return "empty field";
    const literals = new Headers([["X", "1"], ["x", "2"]]);
    if (literals.get("X") !== "1, 2") return "literal pairs";
    const record = {"X-Record":"before"};
    const headers = new Headers(record);
    record["X-Record"] = "after";
    if(headers.get("x-record") !== "before") return "record snapshot";
    const chosen = headers.has("x-record") ? select(headers,"yes") : select(headers,"no");
    if(chosen !== "yes" || headers.get("chosen") !== "yes") return "conditional effects";
    const empty = new Headers();
    const undefinedInit = new Headers(undefined);
    if(empty.has("x") || undefinedInit.has("x")) return "empty init";
    let errors = 0;
    try { empty.set("bad name", "value"); } catch { errors++; }
    try { empty.append("x", "🙂"); } catch { errors++; }
    try { empty.append("x", "a\nb"); } catch { errors++; }
    try { empty.delete(""); } catch { errors++; }
    if(errors !== 4 || empty.has("x")) return "validation";
    for(let i=0;i<500;i++) { headers.set("n", i < 250 ? "early" : "late"); headers.append("transient", "a"); headers.delete("transient"); }
    return headers.get("n") === "late" ? "headers:ok" : "collection";
}
