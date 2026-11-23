// Narrowing by `typeof`, tag and null checks ends where the narrowed local is assigned, and an
// assignment narrows to the assigned alternative.
type Shape = { tag: "circle"; val: number } | { tag: "label"; val: string };
type Holder = { shape: Shape };

function reassigned(x: number | string): number {
  if (typeof x === "number") {
    x = "abc";
    return x.length;
  }
  return 0;
}

function afterEarlyExit(x: number | string): string {
  if (typeof x !== "number") {
    return "early";
  }
  x = "later";
  return `${x}`;
}

function arms(x: number | string, write: boolean): number {
  if (typeof x === "number") {
    if (write) {
      x = "abc";
      return x.length;
    }
    return x + 1;
  }
  return -1;
}

function loopBackEdge(x: number | string): string {
  let out = "";
  if (typeof x === "number") {
    for (let i = 0; i < 3; i++) {
      out += `${x};`;
      x = "s" + i;
    }
  }
  return out;
}

function callbackBackEdge(x: number | string): string {
  let out = "";
  if (typeof x === "number") {
    [1, 2, 3].forEach((n) => {
      out += `${x};`;
      x = "s" + n;
    });
  }
  return out;
}

function loopExit(x: number | string, n: number): string {
  while (n > 0) {
    n--;
    if (typeof x !== "number") {
      break;
    }
  }
  return `${x}`;
}

function catchScope(x: number | string): string {
  try {
    if (typeof x !== "number") {
      throw new Error("not a number");
    }
  } catch (e) {
    return `caught ${x}`;
  }
  return `${x}`;
}

function deferredBranch(flag: boolean): string {
  let value: string;
  if (flag) value = "left";
  else value = "right";
  return value;
}

function deferredCatch(fail: boolean): string {
  let value: string;
  try {
    if (fail) throw new Error("stop");
    value = "assigned";
  } catch {
    return "caught";
  }
  return value;
}

function deferredLoop(count: number): string {
  let value: string | undefined;
  for (let i = 0; i < count; i++) {
    if (i === 0) continue;
    value = "seen";
    break;
  }
  return value === undefined ? "unwritten" : value;
}

function deferredIteration(): number {
  let sum = 0;
  for (const current of [7, 9]) {
    let stored: number | undefined;
    if (stored !== undefined) return -1;
    stored = current;
    sum += stored;
  }
  return sum;
}

function readChunk(fail: boolean): number {
  let chunk;
  const absent = chunk === undefined;
  try {
    if (fail) throw new Error("no bytes");
    chunk = new Uint8Array([7]);
  } catch {
    return 0;
  }
  const output = new Uint8Array(1);
  output.set(chunk, 0);
  return (absent ? 100 : 0) + output[0];
}

class ChunkSource {
  read(): Uint8Array { return new Uint8Array([9]); }
  first(flag: boolean): number {
    let data;
    if (flag) data = this.read();
    else data = new Uint8Array([5]);
    return data[0];
  }
}

function joinedBytes(change: boolean): number {
  let chunk;
  const initiallyAbsent = chunk === undefined;
  if (change) chunk = new Uint8Array([7]);
  else chunk = new Uint8Array([9]);
  const replace = () => { chunk = new Uint8Array([11]); };
  if (change) replace();
  const out = new Uint8Array(1);
  out.set(chunk);
  return (initiallyAbsent ? 100 : 0) + out[0];
}

function joinedLength(bytes: Uint8Array, takeBytes: boolean): number {
  let value;
  if (takeBytes) value = bytes;
  else value = "éé";
  return value.length;
}

function joinedIdentity(bytes: Uint8Array, takeBytes: boolean): number {
  let value;
  if (takeBytes) value = bytes;
  else value = "abc";
  bytes[0] = 41;
  if (typeof value === "object") {
    const same = value === bytes;
    value[0] = value[0] + 1;
    return (same ? 100 : 0) + bytes[0];
  }
  return value.length;
}

function capturedWrite(change: boolean): number {
  let value;
  value = 7;
  const setText = () => { value = "abc"; };
  if (change) setText();
  return String(value).length;
}

function bareRead(write: boolean): boolean {
  let value;
  if (write) value = "present";
  return value === undefined;
}

console.log(deferredBranch(true), deferredBranch(false));
console.log(deferredCatch(false), deferredCatch(true), bareRead(false), bareRead(true));
console.log(deferredLoop(0), deferredLoop(1), deferredLoop(2), deferredIteration());
console.log(readChunk(false), readChunk(true), capturedWrite(false), capturedWrite(true));

const bytes = new Uint8Array(4);
console.log(joinedBytes(false), joinedBytes(true), joinedLength(bytes, false), joinedLength(bytes, true), new ChunkSource().first(true));
console.log(joinedIdentity(bytes, false), joinedIdentity(bytes, true));

function retagged(): string {
  let shape: Shape = { tag: "circle", val: 1 };
  if (shape.tag === "circle") {
    shape = { tag: "label", val: "relabeled" };
    return shape.val;
  }
  return "circle";
}

let retained: Holder | null = null;

function retain(holder: Holder): void {
  retained = holder;
}

function retainedTag(): string {
  if (retained !== null) {
    return retained.shape.tag;
  }
  return "none";
}

function describe(shape: Shape): string {
  return shape.tag === "label" ? shape.val : `circle ${shape.val}`;
}

function aliasWrite(holder: Holder): string {
  const other = holder;
  if (holder.shape.tag === "circle") {
    other.shape = { tag: "label", val: "renamed" };
    return describe(holder.shape);
  }
  return "label";
}

console.log(reassigned(1));
console.log(afterEarlyExit(2));
console.log(arms(1, true), arms(1, false), arms("x", false));
console.log(loopBackEdge(7));
console.log(callbackBackEdge(7));
console.log(loopExit("str", 0), loopExit(3, 2));
console.log(catchScope("str"), catchScope(4));
retain({ shape: { tag: "label", val: "kept" } });
console.log(retagged(), retainedTag());
console.log(aliasWrite({ shape: { tag: "circle", val: 2 } }));

const words = ["ab", "cd"];
let cursor = 0;
function readWord<T>(input: T[]): T | null {
  return cursor < input.length ? input[cursor++] : null;
}
function writeWord<M extends { length: number, toUpperCase(): string }>(message: M) {
  if (message.length <= 2) console.log(message.toUpperCase());
}
for (let message = readWord(words); message !== null; message = readWord(words)) {
  writeWord(message);
}
let lastWord: string | null = null;
cursor = 0;
lastWord = readWord(words);
if (lastWord !== null) console.log("last word length", measured(lastWord));
function measured<M extends { length: number }>(message: M) { return message.length; }
function guardedLength(message: string | null) {
  if (message === null) return 0;
  return measured(message);
}
console.log("guarded lengths", guardedLength("abc"), guardedLength(null));
