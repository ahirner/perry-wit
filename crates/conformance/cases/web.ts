// Headers, Response and Request values, body JSON, aborted timers, Error values and settled promises.
import { setTimeout } from "node:timers/promises";

const h = new Headers({ "X-A": " one " });
h.append("x-a", "twé");
console.log(h.get("X-A"), h.has("x-b"), h.get("x-b") === null);
try {
  h.append("bad name", "v");
} catch (e) {
  if (e instanceof TypeError) console.log(e.message);
}
const r = new Response("hé", { status: 404, statusText: "Nope", headers: h });
console.log(r.status, r.ok, r.statusText, r.headers.get("content-type"), r.headers.get("x-a"));
console.log(await r.text(), r.bodyUsed);
try {
  await r.bytes();
} catch (e) {
  if (e instanceof TypeError) console.log(e.message);
}
try {
  new Response(null, { status: 99 });
} catch (e) {
  if (e instanceof RangeError) console.log(e.message);
}
const q = new Request("HTTPS://Example.TEST:443/a b?x#frag", { method: "post", body: "x" });
console.log(q.method, q.url, q.headers.get("content-type"));
const g = new Request("https://example.test", { method: "patch" });
console.log(g.method, g.url, (await g.bytes()).length, g.bodyUsed);
try {
  new Request("https://example.test", { method: "GET", body: "x" });
} catch (e) {
  if (e instanceof TypeError) console.log(e.message);
}
try {
  new Request("nope");
} catch (e) {
  if (e instanceof TypeError) console.log(e.message);
}

// Object spreads over JSON values, property order, `json()` and array destructuring.
const a = JSON.parse('{"b":1,"2":[1],"1":true,"b":3,"x":{"z":null,"10":"t"}}');
const list = JSON.parse("[7,8]");
console.log(JSON.stringify({ ...a, ...list, x: "y", n: null, ...JSON.parse('"hé"'), ...JSON.parse("5"), u: a.missing, b: false }));
console.log(JSON.stringify(a), JSON.stringify(JSON.parse('{"4294967295":1,"4294967294":2,"01":3,"0":4}')));
const merged = { ...a.x, ...JSON.parse('{"z":2}'), count: 3, ok: true };
const z: number = merged.z;
console.log(JSON.stringify(merged), z, typeof merged.count);
let order = 0;
const ordered = { ...JSON.parse("{}"), first: (order = order + 1), second: (order = order + 1), nil: null };
console.log(JSON.stringify(ordered), order);
const body = new Response('{"k":[1,2.5e-3]}');
console.log(JSON.stringify(await body.json()), body.bodyUsed);
try {
  await new Response('{"k":').json();
} catch (e) {
  if (e instanceof Error) console.log("rejected");
}
// @ts-expect-error destructuring reads past the two settled values.
const [first, second, third] = await Promise.all([new Response("[1]").json(), new Response('{"v":2}').json()]);
console.log(JSON.stringify({ ...first, ...second }), third === undefined);
const numeric = JSON.parse('{"0":0,"41":1,"82":2,"22":3,"63":4,"3":5,"44":6,"85":7,"25":8,"66":9,"6":10,"47":11,"88":12,"28":13,"69":14,"9":15,"50":16,"91":17,"31":18,"72":19,"12":20,"53":21,"94":22,"34":23,"75":24,"15":25,"56":26,"97":27,"37":28,"78":29,"18":30,"59":31,"100":32,"40":33,"81":34,"21":35,"62":36,"2":37,"43":38,"84":39}');
console.log(JSON.stringify(numeric));

// Aborting cancels the timer and rejects it; an aborted signal rejects a new timer at once.
const c = new AbortController();
const t = setTimeout(50, 7, { signal: c.signal });
console.log(c.signal.aborted);
c.abort();
try {
  await t;
  console.log("resolved");
} catch (e) {
  if (e instanceof Error) console.log("rejected", e.message, c.signal.aborted);
}
const d = new AbortController();
console.log(await setTimeout(1, 8, { signal: d.signal }));
d.abort();
try {
  await setTimeout(1, 9, { signal: c.signal });
} catch (e) {
  console.log("pre-aborted");
}

// Error names and causes, plain values among combinator inputs, and error spreads.
const plain = new Error("plain");
const typed = new TypeError("bad", { cause: "root" });
const range = new RangeError("far", { cause: "distance" });
console.log(plain.name, plain.message, plain.cause === undefined, typed.name, typed.cause, range.name, range.cause);
const values = await Promise.all([Promise.resolve(1), 2, 3]);
console.log(values.length, values[0], values[1], values[2]);
const raced = await Promise.race([4, 5]);
const settled = await Promise.allSettled([6, Promise.reject(typed)]);
console.log(raced, settled[0].status, settled[1].status, settled[1].status === "rejected" && settled[1].reason === typed);
try {
  throw typed;
} catch (e) {
  console.log(e === typed, JSON.stringify({ ...typed, extra: 1 }));
}
