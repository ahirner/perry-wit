use super::execute;
use anyhow::Context as _;
use anyhow::Result;

#[tokio::test(flavor = "current_thread")]
async fn callable_instances_preserve_arguments_identity_captures_and_exceptions() -> Result<()> {
    let run = execute(
        r#"
function failure(fn) { try { fn(); return "none"; } catch(e) { return String(e); } }
function twice(f,x) { return f(f(x)); }
function inc(n) { return n+1; }
function apply(f:(n:number)=>number, x:number):number { return f(x); }
function add(a,b) { return a+b; }
function upper(s) { return s.toUpperCase(); }
const alias=inc;
console.log(failure(()=>{throw new Error("boom");}),failure(()=>1),twice(n=>n+1,1));
console.log(alias===inc, alias===add, apply(alias,2), [1,2].map(inc).join(","));
console.log([1,2,3].reduce((acc,v)=>add(acc,v),0), [3,1,2].sort((a,b)=>add(a,-b)).join(","),
    "a-b".replace(/[a-z]/g,m=>upper(m)));
function check(condition,message) { if(!condition) throw new Error(message); }
function contract(value) {
    let checks=0;
    function verify(condition,message) { check(condition,message); checks++; }
    const again=verify;
    again(true,"ok");
    const first=failure(()=>again(value>2,"small"));
    return first+" "+checks;
}
console.log(contract(3),contract(1));
function cb(){console.log("cb");return inc;}
function mark(n:number){console.log(n);return n;}
function sum(f,x:number,...rest:number[]){return f(x)+rest[0];}
function sub(f,x:number,...rest:number[]){return f(x)-rest[0];}
function test(flag:boolean){const fn=flag?sum:sub; console.log(fn(cb(),mark(3),mark(2)));}
console.log(sum(cb(),mark(3),mark(2)));
test(true); test(false);
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "Error: boom none 3\ntrue false 3 2,3\n6 1,2,3 A-B\nnone 2 Error: small 1\ncb\n3\n2\n6\ncb\n3\n2\n6\ncb\n3\n2\n2\n"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn inference_engine_specializes_generics_classes_cells_and_fixed_points() -> Result<()> {
    use std::fmt::Write;
    let mut chain = String::from("function f0(x) { return x + 1; }\n");
    for i in 1..=300 {
        writeln!(chain, "function f{i}(x) {{ return f{}(x) + 1; }}", i - 1)?;
    }
    chain.push_str("console.log(f300(0));\n");

    let mut copies =
        String::from("function inc(x){return x+1;} function twice(x){return 2*x;} const a0=[inc];");
    for i in 1..=1030 {
        writeln!(copies, "const a{i}=Array.from(a{});", i - 1)?;
    }
    copies.push_str("a1030.push(twice); console.log(a0.length, a0[0](4), a1030[1](4));");

    let cases: &[(&str, &str, &str)] = &[
        ("late_write_through_array_copy_chain", &copies, "1 5 8\n"),
        (
            "static_generic_and_implicit_classes",
            r#"
class Util {
  static mark = "u";
  static label(s: string): string { return s + Util.mark; }
  static wrap(x) { return [x]; }
}
class Box {
  constructor(v) { this.v = v; }
  get value() { return this.v; }
  set value(x) { this.v = x; }
  map(f) { return f(this.v); }
  $$perryMethod_map() { return this.v; }
  toString() { return "box"; }
}
console.log("before Pair");
function mark(): number { console.log("Pair initialized"); return 2; }
class Pair {
  static first = mark();
  static mark = Pair.first + 1;
  static scale(n: number): number { return Pair.mark * n; }
  constructor(a, b) { this.a = a; this.b = b; }
  swap() { return new Pair(this.b, this.a); }
}
const bn = new Box(1);
const bs = new Box("a");
console.log(bn.value++, ++bn.value, String(bs));
bn.value = 5;
const swapped = new Pair(1, "x").swap();
console.log(Util.wrap(2)[0], Util.wrap("a")[0] + "!", bn instanceof Box, bs instanceof Box, bn.value, bs.map(s => s + "!"), swapped.a, swapped.b, bs.$$perryMethod_map(), Util.label("x"), Pair.scale(2));
"#,
            "before Pair\nPair initialized\n1 3 box\n2 a! true true 5 a! x 1 a xu 6\n",
        ),
        (
            "class_and_method_type_parameters_have_separate_scopes",
            r#"
class Box<T> {
  value: T;
  constructor(value: T) { this.value = value; }
  contains<T>(value: T) {
    const items = new Set<T>(); items.add(value);
    console.log(this.value); return items.has(value);
  }
}
const box = new Box(3);
console.log(box.contains<string>("x"), box.contains(true));
function outer<T>(value:T){
  function inner<T>(items:T[]){const s=new Set<T>(); s.add(items[0]); console.log(value); return s.has(items[0]);}
  return inner(["x"]);
}
console.log(outer(4));
"#,
            "3\n3\ntrue true\n4\ntrue\n",
        ),
        (
            "generic_promise_settles_after_specialization",
            r#"
async function contains<T>(p: Promise<T>) {
  const value = await p;
  function matches(x: T) { const s = new Set<T>(); s.add(x); return s.has(value); }
  return matches(value);
}
async function source() { return "abc"; }
console.log(await contains(source()));
"#,
            "true\n",
        ),
        (
            "capture_owners_and_mutual_recursion",
            r#"
function addAll(k, arr) { return arr.map(x => x + k); }
function isEven(n) { return n === 0 ? true : isOdd(n - 1); }
function isOdd(n) { return n === 0 ? false : isEven(n - 1); }
console.log(addAll(1, [1, 2]).join(","), addAll("s", ["a", "b"]).join(","), isEven(4), isOdd(3));
"#,
            "2,3 as,bs true true\n",
        ),
        (
            "empty_array_cells_and_reassignment",
            r#"
function fill(list, v) { list.push(v); return list.length; }
function count(list) { return list.length; }
function makeList() { return []; }
class Bag {
  constructor() { this.items = []; }
  add(x) { this.items.push(x); }
}
const c1 = [];
const before = count(c1);
fill(c1, 7);
const c2 = [];
fill(c2, "s");
const f1 = makeList(); f1.push(1);
const f2 = makeList(); f2.push(2);
const state = { items: [] }; state.items.push("x");
const inner = []; inner.push(4);
const outer = []; outer.push(inner);
const bagN = new Bag(); bagN.add(2);
const bagS = new Bag(); bagS.add("k");
const closed = [];
const pushClosed = (v) => { closed.push(v); };
pushClosed(10); pushClosed(20);
let xs = [];
function grow() { xs.push(1); }
function reset() { xs = [2, 3]; }
grow(); reset();
console.log(before, c1[0], c2[0], f1[0] + f2[0], state.items[0], outer[0][0], bagN.items[0], bagS.items[0], closed.length, xs.length, xs[0]);
"#,
            "0 7 s 3 x 4 2 k 2 2 2\n",
        ),
        (
            "write_order_and_late_dependencies",
            r#"
class LateField {
  constructor() { this.items = []; }
  size() { return this.items.length; }
  first() { return this.items[0]; }
  add(v) { this.items.push(v); }
}
const lf = new LateField();
const beforeSize = lf.size();
lf.add("a");
let captured;
const readCap = () => captured;
captured = 2;
let a = null;
function setA1() { a = "x"; }
function setA2() { a = 2; }
setA1(); setA2();
let b = null;
function setB2() { b = 2; }
function setB1() { b = "x"; }
setB1(); setB2();
let c = null;
function late() { return helper(); }
function helper() { c = 3; return 0; }
c = "a";
late();
let mixed;
mixed = 1;
mixed = "a";
mixed = [1];
const ordered = { b: 1, a: "x" };
console.log(beforeSize, lf.size(), lf.first().toUpperCase(), readCap(), typeof a, a, typeof b, b, typeof c, typeof mixed, JSON.stringify(mixed), JSON.stringify(ordered));
"#,
            "0 1 A 2 number 2 number 2 number object [1] {\"b\":1,\"a\":\"x\"}\n",
        ),
        (
            "annotated_fields_keep_declared_types",
            r#"
class Mode { mode: "a" | "b" = "a"; constructor(m: "a" | "b") { this.mode = m; } }
class Late { n = null; set(v) { this.n = v; } }
const late = new Late();
late.set(3);
console.log(new Mode("b").mode, late.n);
"#,
            "b 3\n",
        ),
        ("deep_call_chain_300", chain.as_str(), "301\n"),
        (
            "function_identities_in_storage_and_object_methods",
            r#"
function apply(f, x) { return f(x); }
const twice = (n) => n * 2;
const holder = { f: twice };
const counter = {
  n: 0,
  bump(by) { this.n += by; return this.n; },
  offsets(xs: number[]) { return xs.map(x => x + this.n).join(","); },
};
const greeter = { greet(name) { return "hi " + name; } };
function getFn() { return twice; }
function selfRef() { return selfRef; }
class FnBox { constructor() { this.op = twice; } run(x) { return this.op(x); } }
const fs = [(x) => x + 1];
counter.bump(2);
console.log(counter.offsets([1, 4]));
console.log(
  apply(twice, 4),
  apply(holder.f, 5),
  holder.f(6),
  counter.bump(3),
  greeter.greet("a"),
  getFn()(7),
  selfRef()()() === selfRef,
  new FnBox().run(8),
  fs[0](1),
);
"#,
            "3,6\n8 10 12 5 hi a 14 true 16 2\n",
        ),
        (
            "tagged_function_unions_dispatch_and_callbacks",
            r#"
const events = [];
const handlers = [() => events.push("a"), () => events.push("b")];
handlers.forEach((h) => h());
const inc = (x) => x + 1;
const dbl = (x) => x * 2;
const pick = (flag) => (flag ? inc : dbl);
function applyAll(f, xs) { return xs.map(f); }
const f0 = pick(true);
const f1 = pick(false);
let chosen = inc;
chosen = dbl;
const dynamicHandlers = [];
let added = false;
function addMore() {
  if (!added) {
    added = true;
    dynamicHandlers.push(() => events.push("c"));
  }
}
dynamicHandlers.push(addMore);
for (let i = 0; i < dynamicHandlers.length; i++) {
  dynamicHandlers[i]();
}
class FnHolder {
  constructor(f) { this.f = f; }
  run(x) { return this.f(x); }
}
const h1 = new FnHolder(f0);
const h2 = new FnHolder(f1);
let selections = 0;
function select(flag) { selections++; return pick(flag); }
const empty: number[] = [];
empty.map(select(true));
const mapped = [3, 4].map(select(false));
function counter(start, indexed) {
  let n = start;
  return indexed ? (x, i) => x + i + ++n : x => x + ++n;
}
const short = counter(0, false), long = counter(10, true);
console.log([2, 2].map(short).join(","), [2, 2].map(long).join(","), short(1, 9));
console.log(
  events.join(","),
  f0(3),
  f1(3),
  chosen(4),
  applyAll(f0, [1, 2]).join(","),
  applyAll(f1, [3]).join(","),
  h1.run(5),
  h2.run(5),
  selections, mapped.join(","),
);
"#,
            "3,4 13,15 4\na,b,c 4 6 8 2,3 6 6 10 2 6,8\n",
        ),
        (
            "callable_array_writes_through_aliases_and_parameters",
            r#"
function inc(x: number) { return x + 1; }
function twice(x: number) { return x * 2; }
function scale(n: number) { return (x: number) => x * n; }
function append(fs: ((x: number) => number)[], f) { fs.push(f); }
function forward(fs, f) { append(fs, f); }
function identity(fs) { return fs; }
function replace(fs, f) { fs[0] = f; }
function replaceEither(flag, a, b, f) { (flag ? a : b)[0] = f; }
const a = [inc], b = [inc];
const alias = a;
forward(alias, twice);
forward(identity(b), twice);
const triple = scale(3);
append(a, triple);
replace(identity(b), triple);
console.log(a[0](4), alias[1](4), a[2](4), b[0](4), b[1](4));
console.log(a.length, b.length, a[2] === b[0], b[0] === scale(3));
replaceEither(false, a, b, twice);
const copy = [...a];
copy.push(...[twice]);
const from = Array.from(copy);
from[0] = twice;
console.log(a[0](4), b[0](4), copy[3](4), from[0](4), a.length);
const other = [twice];
replaceEither(false, a, other, triple);
console.log(other[0](4), a[0](4));
const src = [inc];
const filtered = src.slice().filter(() => true);
filtered.push(twice);
const joined = src.concat([triple]).toReversed();
console.log(filtered[1](4), joined[0](4), joined[1](4), src.length);
"#,
            "5 8 12 12 8\n3 2 true false\n5 8 8 8 3\n12 5\n8 12 5 1\n",
        ),
        (
            "escaping_closures_environments_boxed_captures_and_equality",
            r#"
function scale(k) {
  return (x) => x * k;
}
function counter() {
  let n = 0;
  return () => ++n;
}
function pair() {
  let n = 0;
  return [() => ++n, () => n];
}
function boxedParam(n) {
  return () => ++n;
}
function stash(f) {
  const alias = f;
  return [alias];
}
const base = (x) => x + 10;
function pickFn(flag, k) {
  return flag ? scale(k) : base;
}
function selfRet(k) {
  let count = k;
  const f = () => {
    count++;
    return f;
  };
  return { f, get: () => count };
}
function nestedInline(start) {
  let acc = start;
  return (xs) => xs.map((x) => {
    acc += x;
    return acc;
  });
}

const s3 = scale(3);
const s3b = scale(3);
const c1 = counter();
const c2 = counter();
const p = pair();
const bp = boxedParam(10);
const stashed = stash(() => 7);
const u1 = pickFn(true, 4);
const u2 = pickFn(false, 4);
const sr = selfRet(0);
const sf = sr.f;
const sget = sr.get;
const sfAfter = sf()();
const ni = nestedInline(5);

console.log(
  s3(2),
  c1(),
  c1(),
  c2(),
  p[0](),
  p[1](),
  p[0](),
  p[1](),
  bp(),
  bp(),
  stashed[0](),
  [1, 2].map(s3).join(","),
  [1, 2].map(scale(5)).join(","),
  [1, 2].map(u1).join(","),
  [1, 2].map(u2).join(","),
  s3 === s3,
  s3 === s3b,
  s3 !== s3b,
  u1 === u1,
  u1 === u2,
  u2 === u2,
  sfAfter === sf,
  sget(),
  ni([1, 2, 3]).join(","),
);
"#,
            "6 1 2 1 1 1 2 2 11 12 7 3,6 5,10 4,8 11,12 true false true true false true true 2 6,8,11\n",
        ),
        (
            "erased_function_values_preserve_evaluation_order",
            r#"
let trace = "";
function id(n) { return n; }
function other(n) { return -n; }
function get(label) { trace += label; return id; }
function num(label) { trace += label; return 2; }
function apply(a, f, b) { return f(a + b); }
function last(n, f) { return f(n); }
function only(f) { trace += "n"; return f(1); }
function forward(f) { return use(f); }
function use(f) { return f(2) + ["ok"].map(f)[0]; }
console.log(forward(id));
const same = get("a") === get("b");
const distinct = get("c") !== other;
const obj = { first: num("d"), middle: get("e"), last: num("f"), end: get("g") };
const result = apply(num("h"), get("i"), num("j"));
const tail = last(num("k"), get("l"));
only(get("m"));
let reads = 0;
function make(n) { return { x:n, f:function(a) { return this.x + a + n; } }; }
const receiver = make(2);
function getReceiver() { reads++; return receiver; }
getReceiver().f(0);
console.log(getReceiver().f(1), reads);
class Runner { static run(f, n) { return f(n); } set callback(f) { trace += f(3); } }
function choose(flag) { return flag ? id : other; }
console.log(Runner.run(get("o"), num("p")), Runner["run"](choose(false), num("q")));
const runner = new Runner(); runner.callback=get("r");
console.log(trace, same, distinct, obj.first, obj.end(1), result, tail);
"#,
            "2ok\n5 2\n2 -2\nabcdefghijklmnopqr3 true true 2 1 4 2\n",
        ),
        (
            "loop_let_boxes_nullable_functions_typeof_and_wide_objects",
            r#"
function makeLoopFns() {
  const fs = [];
  for (let i = 0; i < 3; i++) {
    fs.push(() => i);
    i += 0;
  }
  return fs;
}
const modLoopFns = [];
for (let i = 0; i < 3; i++) modLoopFns.push(() => i);

function maybeFn(b) { return b ? () => 1 : null; }
function runOpt(cb) { if (cb) return cb(2); return 0; }
const mf1 = maybeFn(true);
const mf2 = maybeFn(false);
let ifRes = -1;
if (mf1) ifRes = mf1();
let unguardedErr = "none";
try { mf2(); } catch (e) { unguardedErr = "TypeError"; }

const f0 = () => 1;
const arr0 = [f0];
const wide = {
  p0: 0, p1: 1, p2: 2, p3: 3, p4: 4, p5: 5, p6: 6, p7: 7, p8: 8, p9: 9,
  p10: 10, p11: 11, p12: 12, p13: 13, p14: 14, p15: 15, p16: 16, p17: 17, p18: 18, p19: 19,
  p20: 20, p21: 21, p22: 22, p23: 23, p24: 24, p25: 25, p26: 26, p27: 27, p28: 28, p29: 29,
  p30: 30, p31: 31, p32: 32, p33: 33, p34: 34, p35: 35, p36: 36, p37: 37, p38: 38, p39: 39,
  p40: 40, p41: 41, p42: 42, p43: 43, p44: 44, p45: 45, p46: 46, p47: 47, p48: 48, p49: 49,
  p50: 50, p51: 51, p52: 52, p53: 53, p54: 54, p55: 55, p56: 56, p57: 57, p58: 58, p59: 59,
  p60: 60, p61: 61, p62: 62, p63: 63, p64: 64, p65: 65, p66: 66, p67: 67, p68: 68, p69: 69,
  m: f0,
};
console.log(
  makeLoopFns().map((f) => f()).join(","),
  modLoopFns.map((f) => f()).join(","),
  mf1 ? mf1() : -1,
  mf2 ? mf2() : -1,
  ifRes,
  runOpt((x) => x * 3),
  runOpt(undefined),
  unguardedErr,
  typeof f0,
  typeof arr0[0],
  typeof wide.m,
  typeof mf1,
  typeof mf2,
  wide.p69,
  wide.m(),
);
"#,
            "0,1,2 0,1,2 1 -1 1 6 0 TypeError function function function function object 69 1\n",
        ),
        (
            "recursive_env_classes_identity_widening_and_class_field_init",
            r#"
function wrap(f, k) { return () => f() + k; }
function build(n, base) {
  let g = base;
  for (let i = 1; i <= n; i++) g = wrap(g, i);
  return g;
}
function upTo(limit) {
  const step = (i) => (i >= limit ? i : step(i + 1));
  return step;
}
function parity() {
  const even = (n) => (n === 0 ? true : odd(n - 1));
  const odd = (n) => (n === 0 ? false : even(n - 1));
  return [even, odd];
}
function runHandlers() {
  const handlers = [];
  let count = 0;
  const on = (h) => handlers.push(h);
  on(() => { count++; if (count < 3) on(() => { count += 10; }); });
  for (let i = 0; i < handlers.length; i++) handlers[i]();
  return count + ":" + handlers.length;
}
let calls = 0;
function mk(k) { calls++; return () => k; }
function mk2(k) { calls++; return () => k * 10; }
class Holder {
  h = mk(1);
  constructor(b) { if (b) this.h = mk2(2); }
}
const plainWide = {
  p0: 0, p1: 1, p2: 2, p3: 3, p4: 4, p5: 5, p6: 6, p7: 7, p8: 8, p9: 9,
  p10: 10, p11: 11, p12: 12, p13: 13, p14: 14, p15: 15, p16: 16, p17: 17, p18: 18, p19: 19,
  p20: 20, p21: 21, p22: 22, p23: 23, p24: 24, p25: 25, p26: 26, p27: 27, p28: 28, p29: 29,
  p30: 30, p31: 31, p32: 32, p33: 33, p34: 34, p35: 35, p36: 36, p37: 37, p38: 38, p39: 39,
  p40: 40, p41: 41, p42: 42, p43: 43, p44: 44, p45: 45, p46: 46, p47: 47, p48: 48, p49: 49,
  p50: 50, p51: 51, p52: 52, p53: 53, p54: 54, p55: 55, p56: 56, p57: 57, p58: 58, p59: 59,
  p60: 60, p61: 61, p62: 62, p63: 63, p64: 64, p65: 65, p66: 66, p67: 67, p68: 68, p69: 69,
};
const [e, o] = parity();
const zero = () => 0;
function counter(n) { return () => ++n; }
const next = counter(10);
const left = build(2, next);
const right = build(2, next);
const saved = [left, right, next];
console.log(saved[0] === left, left === right, [1, 2].map(left).join(","), next(), right(), saved[0]());
console.log(
  build(3, zero)(),
  build(4, zero)(),
  upTo(5)(0),
  e(4),
  o(4),
  e(3),
  runHandlers(),
  new Holder(true).h(),
  new Holder(false).h(),
  calls,
  plainWide.p69,
  [() => 11, () => 22][1](),
);
"#,
            "true false 14,15 13 17 18\n6 10 5 true false false 11:2 20 1 3 69 22\n",
        ),
        (
            "promise_operand_order_and_finally_rejection",
            r#"
let trace = "";
function identity(value) { return value; }
function handler() { trace += "h"; return identity; }
function addOne(value) { return value + 1; }
function optional(flag) { return flag ? addOne : null; }
function extra(tag) { trace += tag; return 0; }
function executor(resolve) { trace += "x"; resolve(4, extra("s")); }
function makeExecutor() { trace += "e"; return executor; }
function optionalExecutor(flag) { return flag ? executor : null; }
class Ticket {
  ready: Promise<number> = new Promise<number>(resolve => resolve(6)).then(n => n + 1);
}
async function verify() {
  try { new Promise(optionalExecutor(false)); } catch (e) { trace += "t"; }
  const p = new Promise(makeExecutor(), extra("a"));
  const value = await p.then(handler(), null, extra("b")).then(17);
  const clear = () => clearTimeout(undefined, extra("c"));
  clear();
  const rejection = await p.finally(() => Promise.reject(new Error("override"))).catch(e => e.message);
  console.log(trace, value, rejection, await p.then(optional(false)), await p.then(optional(true)),
    await new Ticket().ready, clearTimeout(undefined) === undefined);
}
await verify();
"#,
            "teaxshbc 4 override 4 5 7 true\n",
        ),
        (
            "conditional_evaluation_in_fields_defaults_and_lazy_operands",
            r#"
let calls = 0;
function hit(value: number): number { calls++; return value; }
function fallback(value: number | undefined = undefined) {
  if (value == undefined) value = 19;
  return value ?? 0;
}
console.log(fallback(), fallback(4));
class Counter {
  seed = hit(5);
  field = 1 + (this.seed === 5 ? hit(this.seed) : hit(100));
  read(value = 1 + (this.seed === 5 ? hit(this.field) : hit(100))) { return value; }
}
async function lazy(run: boolean, c: Counter) {
  return run && (1 + (c.seed === 5 ? await Promise.resolve(hit(c.field)) : hit(100))) > 0;
}
const c = new Counter();
console.log(c.field, calls);
console.log(c.read(20), calls);
console.log(c.read(), calls);
console.log(await lazy(false, c), calls);
console.log(await lazy(true, c), calls);
function read(a: number[]) {
  const x = a[0], n = a.length;
  const result: number | string = x ?? "missing";
  console.log(n === 0 ? x : 7, x && 1, n && x, x || 3, n || x, x ?? 4, undefined ?? x, result, x && "ok");
}
read([]);
read([0]);
read([6]);
class Bucket<T> {
  values: T[] = [];
  selected = this.values.length === 0 ? this.values : [];
  constructor(value: T) { this.selected.push(value); }
}
console.log(new Bucket<number>(42).selected[0], new Bucket<string>("ok").selected[0]);
"#,
            "19 4\n6 2\n20 2\n7 3\nfalse 3\ntrue 4\nundefined undefined 0 3 undefined 4 undefined missing undefined\n7 0 0 3 1 0 0 0 0\n7 1 6 6 1 6 6 6 ok\n42 ok\n",
        ),
        (
            "promise_handles_keep_their_constructor_instance",
            r#"
function job(value) {
  let send;
  const promise = new Promise(resolve => { send = resolve; });
  return { send: () => send(value), promise };
}
const first = job(42);
const second = job("ok");
second.send();
first.send();
console.log(await first.promise, await second.promise);
"#,
            "42 ok\n",
        ),
        (
            "guest_settled_promises_and_global_timers",
            r#"
type __PerryTimer = { label: string };
function readLabel(value: __PerryTimer) { return value.label; }
async function main() {
  const immediate = await new Promise((resolve) => resolve(41 + 1));
  const delayed = await new Promise((resolve) => {
    setTimeout(() => resolve("later"), 5);
  });
  const withArgs = await new Promise((resolve) => {
    setTimeout((a, b) => resolve(a + b), 2, 10, 20);
  });
  const caught = await new Promise((_, reject) => {
    reject(new Error("boom"));
  }).catch((e) => e.message);
  const rejected = new Promise((_, reject) => reject(new Error("forwarded")));
  const forwarded = await rejected.then(n => n).then().catch(e => e.message);
  const thrown = await new Promise(() => {
    throw new Error("sync-fail");
  }).catch((e) => e.message);
  const fromHandler = await new Promise(r => r(1)).then(() => { throw new Error("reaction"); }).catch(e => e.message);
  const chained = await new Promise((resolve) => resolve(3))
    .then((n) => new Promise((resolve) => setTimeout(() => resolve(n * 4), 2)))
    .then((n) => n + 1);
  const log = [];
  const finOk = await new Promise((resolve) => resolve("ok")).finally(() => {
    log.push("f1");
  });
  const finErr = await new Promise((_, reject) => reject(new Error("bad")))
    .finally(() => {
      log.push("f2");
    })
    .catch((e) => e.message);

  let cancelledRan = false;
  const cancelled = setTimeout(() => {
    cancelledRan = true;
  }, 50);
  setTimeout(() => clearTimeout(cancelled), 1);

  const ticks = await new Promise((resolve) => {
    let count = 0;
    let id;
    id = setInterval(() => {
      count++;
      if (count === 3) {
        clearInterval(id);
        resolve(count);
      }
    }, 2);
  });

  const thisMatches = await new Promise((resolve) => {
    let handle;
    handle = setTimeout(function () {
      resolve(this === handle);
    }, 2);
  });

  const imm = await new Promise((resolve) => {
    setImmediate((v) => resolve(v), "imm");
  });

  console.log(
    immediate,
    delayed,
    withArgs,
    caught,
    forwarded,
    fromHandler,
    thrown,
    chained,
    finOk,
    finErr,
    log.join(","),
    cancelledRan,
    ticks,
    thisMatches,
    imm,
    readLabel({label: "user"}),
  );
}
await main();
"#,
            "42 later 30 boom forwarded reaction sync-fail 13 ok bad f1,f2 false 3 true imm user\n",
        ),
    ];
    for &(name, source, expected) in cases {
        let run = execute(source)
            .await
            .with_context(|| format!("compiling case {name}"))?;
        assert!(run.ok, "case {name} failed: {}", run.stderr);
        assert_eq!(run.stdout, expected, "case {name} output mismatch");
    }
    let parameters = (0..64)
        .map(|i| format!("a{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let arguments = (0..64).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let run = execute(&format!("function f({parameters}, cb) {{ return cb(a63); }} console.log(f({arguments}, n => n + 1));")).await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout, "64\n");
    Ok(())
}
