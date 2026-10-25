class SeparateKeys {
  keys: string[] = [];
  values: number[] = [];

  lookup(key: string): number | undefined {
    for (let i = 0; i < this.keys.length; i++) {
      if (this.keys[i] === key) return this.values[i];
    }
    return undefined;
  }

  assign(key: string, value: number): void {
    for (let i = 0; i < this.keys.length; i++) {
      if (this.keys[i] === key) {
        this.values[i] = value;
        return;
      }
    }
    this.keys[this.keys.length] = key;
    this.values[this.values.length] = value;
  }
}

function word(key: string, start: number): number {
  let result = 0;
  for (let i = start; i < key.length && i < start + 4; i++) {
    result |= (key.charCodeAt(i) & 255) << ((i - start) * 8);
  }
  return result;
}

function exactKey(key: string): boolean {
  if (key.length > 8) return false;
  for (let i = 0; i < key.length; i++) {
    if (key.charCodeAt(i) > 255) return false;
  }
  return true;
}

class CompactKeys {
  keys: string[] = [];
  low: number[] = [];
  high: number[] = [];
  lengths: number[] = [];
  exact: boolean[] = [];
  values: number[] = [];

  find(key: string, low: number, high: number, exact: boolean): number {
    for (let i = 0; i < this.low.length; i++) {
      const candidateLow = this.low[i];
      const candidateHigh = this.high[i];
      const length = this.lengths[i];
      if (candidateLow === undefined || candidateHigh === undefined || length === undefined) throw new Error("key storage mismatch");
      if (candidateLow === low && candidateHigh === high && length === key.length) {
        if ((exact && this.exact[i]) || this.keys[i] === key) return i;
      }
    }
    return -1;
  }

  lookup(key: string): number | undefined {
    const index = this.find(key, word(key, 0), word(key, 4), exactKey(key));
    if (index < 0) return undefined;
    return this.values[index];
  }

  assign(key: string, value: number): void {
    const low = word(key, 0);
    const high = word(key, 4);
    const exact = exactKey(key);
    const index = this.find(key, low, high, exact);
    if (index >= 0) {
      this.values[index] = value;
      return;
    }
    this.keys[this.keys.length] = key;
    this.low[this.low.length] = low;
    this.high[this.high.length] = high;
    this.lengths[this.lengths.length] = key.length;
    this.exact[this.exact.length] = exact;
    this.values[this.values.length] = value;
  }
}

let interned = new CompactKeys();
let nextKey = 0;

function intern(key: string): number {
  const known = interned.lookup(key);
  if (known !== undefined) return known;
  const id = nextKey++;
  interned.assign(key, id);
  return id;
}

class InternedKeys {
  keys: number[] = [];
  values: number[] = [];

  lookup(key: string): number | undefined {
    const id = intern(key);
    for (let i = 0; i < this.keys.length; i++) {
      if (this.keys[i] === id) return this.values[i];
    }
    return undefined;
  }

  assign(key: string, value: number): void {
    const id = intern(key);
    for (let i = 0; i < this.keys.length; i++) {
      if (this.keys[i] === id) {
        this.values[i] = value;
        return;
      }
    }
    this.keys[this.keys.length] = id;
    this.values[this.values.length] = value;
  }
}

class Shape {
  keys: number[] = [];
  transitionKeys: number[] = [];
  transitionTargets: number[] = [];
}

let shapes: Shape[] = [new Shape()];

class SharedShapes {
  shape: number = 0;
  values: number[] = [];

  lookup(key: string): number | undefined {
    const id = intern(key);
    const shape = shapes[this.shape];
    for (let i = 0; i < shape.keys.length; i++) {
      if (shape.keys[i] === id) return this.values[i];
    }
    return undefined;
  }

  assign(key: string, value: number): void {
    const id = intern(key);
    const shape = shapes[this.shape];
    for (let i = 0; i < shape.transitionKeys.length; i++) {
      if (shape.transitionKeys[i] === id) {
        this.shape = shape.transitionTargets[i];
        this.values[this.values.length] = value;
        return;
      }
    }
    for (let i = 0; i < shape.keys.length; i++) {
      if (shape.keys[i] === id) {
        this.values[i] = value;
        return;
      }
    }
    const next = new Shape();
    for (let i = 0; i < shape.keys.length; i++) next.keys[next.keys.length] = shape.keys[i];
    next.keys[next.keys.length] = id;
    this.shape = shapes.length;
    shape.transitionKeys[shape.transitionKeys.length] = id;
    shape.transitionTargets[shape.transitionTargets.length] = this.shape;
    shapes.push(next);
    this.values[this.values.length] = value;
  }
}

class Dictionary {
  values: Record<string, number> = {};
  lookup(key: string): number | undefined { return this.values[key]; }
  assign(key: string, value: number): void { this.values[key] = value; }
}

function verify(): void {
  const a = new __STORE__();
  const b = new __STORE__();
  const keys = ["field_0", "field_19", "", "a\0b", "12345678a", "12345678b", "é", "雪", "😀", "\u0001", "ā"];
  for (let i = 0; i < keys.length; i++) {
    a.assign(keys[i], i);
    b.assign(keys[keys.length - i - 1], keys.length - i - 1);
  }
  a.assign("field_0", 99);
  for (let i = 0; i < keys.length; i++) {
    const first = a.lookup(keys[i]);
    const second = b.lookup(keys[i]);
    if (first === undefined || second === undefined) throw new Error("absent property");
    if (first !== (i === 0 ? 99 : i) || second !== i) throw new Error("property mismatch");
  }
  if (a.lookup("missing") !== undefined) throw new Error("missing key");
}

function workload(objects: number, fields: number, reverse: boolean): number {
  let checksum = 0;
  for (let i = 0; i < objects; i++) {
    const object = new __STORE__();
    for (let j = 0; j < fields; j++) {
      const field = reverse && i % 2 !== 0 ? fields - j - 1 : j;
      object.assign("field_" + field, i * fields + field);
    }
    const first = object.lookup("field_0");
    const last = object.lookup("field_" + (fields - 1));
    if (first === undefined || last === undefined) throw new Error("absent benchmark property");
    checksum += first + last;
  }
  return checksum;
}

verify();
interned = new CompactKeys();
nextKey = 0;
shapes = [new Shape()];
workload(100, __FIELDS__, __REVERSE__);
const start = Date.now();
const checksum = workload(__OBJECTS__, __FIELDS__, __REVERSE__);
console.log("elapsed:" + (Date.now() - start));
console.log("checksum:" + checksum);
