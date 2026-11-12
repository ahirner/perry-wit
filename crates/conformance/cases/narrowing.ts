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
