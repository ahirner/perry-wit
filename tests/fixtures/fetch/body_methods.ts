async function retain(buffer: ArrayBuffer): Promise<ArrayBuffer> {
    await 0;
    return buffer;
}

export async function run(base: string): Promise<string> {
    const response = await fetch(base + "/json");
    const pending = response.json();
    const data = await pending;
    const repeated = await pending;
    if (data !== repeated || !response.bodyUsed) return "json identity";
    const label = data.label;
    if (typeof label !== "string") return "json shape";
    const binary = await fetch(base + "/bytes");
    const buffer = binary.arrayBuffer();
    const value: ArrayBuffer = await buffer;
    if ((await buffer) !== value || value.byteLength !== 3) return "buffer identity";
    const owner = {buffer: value};
    const retained = retain(value);
    const first = new Uint8Array(value);
    for (let i = 0; i < 2000; i++) { const garbage = base.toLowerCase().split(".").join("-"); }
    if (owner.buffer !== value || (await retained) !== value) return "lost buffer owner";
    const second = new Uint8Array(value);
    if (first === second) return "view identity";
    first[1] = 42;
    if (second[1] !== 42 || first[0] !== 0 || first[2] !== 255) return "view alias";
    let failures = 0;
    try { await binary.json(); } catch (error) { failures++; }
    const invalid = await fetch(base + "/invalid");
    try { await invalid.json(); } catch (error) { failures++; }
    if (!invalid.bodyUsed) return "invalid body unused";
    if (failures !== 2) return "missing failure";
    return label + ":2";
}
