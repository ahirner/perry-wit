// Number formatting, radix conversion, Math, bigint and Uint8Array element conversion.

for (const value of [-0, NaN, Infinity, -Infinity, 1e-7, 1e-6, 1e20, 1e21]) {
  console.log("coercion", value, String(value), "" + value, `${value}`);
}
console.log("radix", (255).toString(16), (-10).toString(2), (18446744073709551616).toString(16), (0.1).toString(3));
console.log("radix-tiny", (1e-20).toString(2));

const b1: bigint = 100n;
const b2: bigint = 35n;
console.log("bigint", String(b1 - b2), String(b1 + b2), String(b1 * 2n), String(b1 / b2), String(b1 % b2), b1 > b2, b2 < b1);

for (const x of [0, 1, -0.5, 2.5, -2.5, 7, NaN]) {
  console.log("math", `${x} ${Math.floor(x)} ${Math.ceil(x)} ${Math.trunc(x)} ${Math.abs(x)} ${Math.round(x)} ${Math.min(x, 3)} ${Math.max(x, -1)}`);
  const bytes = new Uint8Array([x, 256 + x, -1, 1.7]);
  const sub = bytes.subarray(1, 3);
  sub[0] = 300;
  const copy = new Uint8Array(bytes);
  copy.set(sub, 2);
  let joined = "";
  for (let i = 0; i < copy.length; i++) joined += `${copy[i]},`;
  console.log("bytes", joined, bytes[9] === undefined, sub.byteLength);
}

function signedZero(value: number): string {
  return value === 0 && 1 / value < 0 ? "-0" : `${value}`;
}
console.log("rounded-zero", signedZero(Math.round(-0.25)), signedZero(Math.ceil(-0.5)), Math.trunc(-0.5));

function scalarStep(acc: number, i: number): number {
  return (acc + i * 3) % 1000003;
}

function scalarLoops(x: number): number {
  let acc = 0;
  for (let i = 0; i < x; i++) {
  acc = scalarStep(acc, i);
  }
  let k = 0;
  while (true) {
  k++;
  if (k > 10) { break; }
  if (k % 2 === 0) { continue; }
  acc = acc - 1;
  }
  return acc;
}
for (const x of [0, 1, 10, 1000, 7.5]) console.log("scalar-loops", x, scalarLoops(x));
