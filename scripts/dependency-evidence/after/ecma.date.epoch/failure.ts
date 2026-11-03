
const sharedJsonCell: { value: number } = { value: 0 };
function clobberSharedJsonCell(): void {
    sharedJsonCell.value = -999;
}
function typedJson(value: number): number {
    sharedJsonCell.value = value;
    const snapshot = JSON.stringify(sharedJsonCell);
    clobberSharedJsonCell();
    const preserved: { value: number | null } = JSON.parse(snapshot);
    const parsed: { value: number | null; rows: number[] } = JSON.parse(JSON.stringify({ value: preserved.value, rows: [1, 2, 3] }));
    parsed.rows[0] = 4;
    return parsed.value === null ? 0 : parsed.value;
}
function byteAlias(first: number, second: number): number {
    const bytes = new Uint8Array([first, second]);
    const alias = bytes.subarray(1);
    alias[0] = first;
    return bytes[1] as number;
}
function closedLoop(value: number, count: number): number {
    const state = { value: value, step: 0 };
    for (let i = 0; i < count; i++) {
        if (i % 2 === 0) state.value += i;
        else state.value -= i;
        state.step++;
    }
    return state.value;
}
function handled(value: number): number {
    try { if (value < 0) throw value; return value; }
    catch (error) { return error as number; }
}

function mark(trace:number[], value:number):number { trace.push(value); return value; }
function identity(value:number):number { return value; }
export function run(x:number):{value:number,trace:number[]} {
  const trace:number[] = [];
  const value = new Date((new Uint8Array([x, x, (new TextEncoder().encode("").subarray(x, identity(JSON.parse(JSON.stringify({text:""})).text.length)).byteLength)])[2])).getTime();
  return {value:value, trace:trace};
}
