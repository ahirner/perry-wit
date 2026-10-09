//! End to end: TypeScript → HIR → MIR → wasm dialect → core Wasm linked with
//! `perry-runtime`, executed in wasmtime with fuel metering (raw exports, no component).
use perry_codegen::status_code;
use perry_mir::attrs::ErrorKind;
use wasmtime::{Config, Engine, Instance, Module, Store, Val};

fn build(source: &str) -> Vec<u8> {
    perry_wit::compile_core(source, "case.ts").unwrap_or_else(|e| panic!("{e:#}"))
}

struct Program {
    store: Store<()>,
    instance: Instance,
    wasm: Vec<u8>,
}

impl Program {
    fn new(source: &str) -> Self {
        let wasm = build(source);
        wasmparser::Validator::new()
            .validate_all(&wasm)
            .unwrap_or_else(|e| panic!("{e}\n{}", wasmprinter::print_bytes(&wasm).unwrap()));
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).unwrap();
        let mut store = Store::new(&engine, ());
        store.set_fuel(u64::MAX).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        assert_eq!(module.imports().len(), 0, "the runtime is linked in");
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        Program {
            store,
            instance,
            wasm,
        }
    }

    /// Call an export; returns its results and the fuel the call consumed.
    fn call(&mut self, name: &str, args: &[Val]) -> (Vec<Val>, u64) {
        let function = self.instance.get_func(&mut self.store, name).unwrap();
        let mut results = vec![Val::I32(0); function.ty(&self.store).results().len()];
        let before = self.store.get_fuel().unwrap();
        function
            .call(&mut self.store, args, &mut results)
            .unwrap_or_else(|e| panic!("{e:?}\n{}", wasmprinter::print_bytes(&self.wasm).unwrap()));
        (results, before - self.store.get_fuel().unwrap())
    }

    fn number(&mut self, name: &str, x: f64) -> f64 {
        let (results, _) = self.call(name, &[Val::F64(x.to_bits())]);
        results[0].unwrap_f64()
    }

    /// `(value, status)` of an export that may throw.
    fn throwing(&mut self, name: &str, x: f64) -> (f64, i32) {
        let (results, _) = self.call(name, &[Val::F64(x.to_bits())]);
        assert_eq!(results.len(), 2, "expected a status result");
        (results[0].unwrap_f64(), results[1].unwrap_i32())
    }
}

#[test]
fn scalar_loops_calls_and_remainder() {
    let mut program = Program::new(
        r#"
function step(acc: number, i: number): number {
  return (acc + i * 3) % 1000003;
}
export function run(x: number): number {
  let acc = 0;
  for (let i = 0; i < x; i++) {
    acc = step(acc, i);
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
"#,
    );
    let reference = |x: f64| {
        let mut acc = 0.0;
        let mut i = 0.0;
        while i < x {
            acc = (acc + i * 3.0) % 1000003.0;
            i += 1.0;
        }
        acc - 5.0
    };
    for x in [0.0, 1.0, 10.0, 1000.0] {
        assert_eq!(program.number("run", x), reference(x), "x={x}");
    }
    assert_eq!(program.number("run", 7.5), reference(7.5));
}

#[test]
fn records_arrays_and_holes() {
    let mut program = Program::new(
        r#"
interface Point { x: number; y: number }
function make(i: number): Point {
  return { y: i * 2, x: i };
}
export function run(x: number): number {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
    items.push(make(i));
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    const point = items[k];
    point.x = point.x + 1;
    sum = (sum + point.x + point.y) % 1000003;
  }
  const pair: number[] = [sum, x];
  pair[1] = pair[0];
  pair[2] = 5;
  return pair[1] + pair[2];
}
"#,
    );
    let reference = |x: f64| {
        let mut sum = 0.0;
        let mut i = 0.0;
        while i < x {
            sum = (sum + (i + 1.0) + i * 2.0) % 1000003.0;
            i += 1.0;
        }
        sum + 5.0
    };
    for x in [0.0, 3.0, 100.0, 5000.0] {
        assert_eq!(program.throwing("run", x), (reference(x), 0), "x={x}");
    }
}

#[test]
fn out_of_bounds_reads_are_undefined_and_typed_uses_throw() {
    let mut program = Program::new(
        r#"
interface Point { x: number; y: number }
export function lookup(n: number): number {
  const xs: number[] = [1, 2];
  const missing = xs[n];
  if (missing === undefined) {
    return -1;
  }
  return missing * 10;
}
export function nan(n: number): number {
  const xs: number[] = [1, 2];
  return xs[n] * 2;
}
export function field(n: number): number {
  const points: Point[] = [{ x: 4, y: 5 }];
  const p = points[n];
  return p.y;
}
export function hole(n: number): number {
  const xs: number[] = [1];
  xs[n] = 3;
  return xs.length;
}
"#,
    );
    assert_eq!(program.number("lookup", 1.0), 20.0);
    for missing in [2.0, -1.0, 0.5, 1e10, f64::NAN] {
        assert_eq!(program.number("lookup", missing), -1.0, "index {missing}");
    }
    assert_eq!(program.number("lookup", -0.0), 10.0);
    assert_eq!(program.number("nan", 0.0), 2.0);
    assert!(program.number("nan", 9.0).is_nan());
    // `undefined.y` is a catchable TypeError, reported as a status, not a trap.
    assert_eq!(program.throwing("field", 0.0), (5.0, 0));
    assert_eq!(
        program.throwing("field", 1.0),
        (0.0, status_code(ErrorKind::TypeError))
    );
    // Writing at `length` appends; anything further out is a RangeError.
    assert_eq!(program.throwing("hole", 0.0), (1.0, 0));
    assert_eq!(program.throwing("hole", 1.0), (2.0, 0));
    assert_eq!(
        program.throwing("hole", 3.0).1,
        status_code(ErrorKind::RangeError)
    );
}

/// The P0.1 scaling contracts that fail on the legacy backend, on the MIR backend: fuel for
/// 4n must stay within 6× of fuel for n (warm-up n first, one instance, as in
/// `crates/conformance/src/scaling.rs`). Values are returned directly; the `{ value, trace }`
/// record needs the component boundary (P6.1).
/// Async functions awaiting each other without a host: values live across awaits stay in the
/// frame, loops run through awaits, and a rejection propagates as the export's status.
#[test]
fn async_functions_await_each_other() {
    let mut program = Program::new(
        r#"
async function double(x: number): Promise<number> {
  return x * 2;
}
async function check(x: number): Promise<number> {
  const xs: number[] = [1];
  return xs[x] as number;
}
async function sum(n: number): Promise<number> {
  let total = 0;
  const offset = n * 100;
  for (let i = 0; i < n; i++) {
    total = total + (await double(i));
  }
  return total + offset;
}
export async function run(n: number): Promise<number> {
  const a = await sum(n);
  const b = await double(a);
  return a + b;
}
export async function fails(n: number): Promise<number> {
  const before = n + 1;
  const v = await check(n);
  return before + v;
}
"#,
    );
    // sum(4) = 2 * (0 + 1 + 2 + 3) + 400 = 412; run = 412 + 824.
    assert_eq!(program.throwing("run", 4.0), (1236.0, 0));
    assert_eq!(program.throwing("run", 0.0), (0.0, 0));
    assert_eq!(program.throwing("fails", 0.0), (2.0, 0));
    assert_eq!(
        program.throwing("fails", 3.0).1,
        status_code(ErrorKind::TypeError)
    );
}

#[test]
fn returned_promises_settle_after_finalizers_outside_lexical_catches() {
    let mut program = Program::new(
        r#"
async function ready(value:number):Promise<number>{return value;}
async function reject(error:Error):Promise<number>{throw error;}
async function nothing():Promise<void>{}
async function forwardVoid():Promise<void>{return nothing();}
function relay(pending:Promise<number>):Promise<number>{return pending;}
async function forward(pending:Promise<number>,trace:number[]):Promise<number>{
  try {return relay(pending);}
  catch(e){trace.push(99);return -1;}
  finally{trace.push(1);}
}
async function explicit(pending:Promise<number>,trace:number[]):Promise<number>{
  try {return await pending;}
  catch(e){trace.push(3);return 40;}
  finally{trace.push(4);}
}
async function replace(pending:Promise<number>):Promise<number>{
  try{return pending;}finally{return ready(8);}
}
async function replaceWithError(pending:Promise<number>,error:Error):Promise<number>{
  try{return pending;}finally{throw error;}
}
async function choose(flag:boolean,pending:Promise<number>,trace:number[]):Promise<number>{
  try{return flag?pending:9;}finally{trace.push(5);}
}
async function nested(pending:Promise<number>,trace:number[]):Promise<number>{
  try{try{return pending;}finally{trace.push(6);await nothing();}}
  finally{trace.push(7);}
}
export async function run(flag:number):Promise<number>{
  const error=new Error('original');
  const pending=reject(error);
  const trace:number[]=[];
  const forwarded=forward(pending,trace);
  let caught=false;
  try{await forwarded;}catch(e){caught=e===error;trace.push(2);}
  if(!caught || await explicit(pending,trace)!==40)throw 1;
  if(await replace(pending)!==8)throw 2;
  const replacement=new Error('replacement');
  caught=false;
  try{await replaceWithError(pending,replacement);}catch(e){caught=e===replacement;}
  if(!caught)throw 3;
  if(await choose(flag>0,ready(10),trace)!==(flag>0?10:9))throw 4;
  if(await nested(ready(11),trace)!==11)throw 5;
  await forwardVoid();
  if(trace.length!==7)throw 6;
  return trace[0]*1000000+trace[1]*100000+trace[2]*10000+trace[3]*1000+trace[4]*100+trace[5]*10+trace[6];
}
"#,
    );
    for flag in [0.0, 1.0] {
        assert_eq!(program.throwing("run", flag), (1234567.0, 0));
    }
}

#[test]
fn adopted_json_result_checks_happen_after_finalizers_and_bypass_local_catch() {
    let mut program = Program::new(
        r#"
type JsonValue=null|boolean|number|string|JsonValue[]|{[key:string]:JsonValue};
async function wrong():Promise<JsonValue>{return JSON.parse('"wrong"');}
async function direct(pending:Promise<JsonValue>,trace:number[]):Promise<number>{
  try{return pending;}
  catch(e){trace.push(99);return 0;}
  finally{trace.push(1);}
}
export async function run(x:number):Promise<number>{
  const trace:number[]=[];
  let caught=false;
  try{await direct(wrong(),trace);}catch(e){caught=e instanceof TypeError;trace.push(2);}
  if(!caught || trace.length!==2)throw new Error('adoption used a stale handler');
  return trace[0]*10+trace[1];
}
"#,
    );
    assert_eq!(program.throwing("run", 0.0), (12.0, 0));
}

#[test]
fn returned_promises_pack_json_payloads_after_finalizers() {
    let mut program = Program::new(
        r#"
type JsonValue=null|boolean|number|string|JsonValue[]|{[key:string]:JsonValue};
interface Box { count:number }
async function ready(value:number):Promise<number>{return value;}
async function native(value:Box):Promise<Box>{return value;}
async function parsed():Promise<JsonValue>{return JSON.parse('{"count":3}');}
async function reject(error:Error):Promise<JsonValue>{throw error;}
async function liftNumber(value:number):Promise<JsonValue>{return ready(value);}
async function liftRecord(value:Box):Promise<JsonValue>{
  try{return native(value);}finally{await ready(0);value.count=value.count+1;}
}
async function liftParsed():Promise<JsonValue>{return parsed();}
async function choose(flag:boolean):Promise<JsonValue>{return flag?ready(7):[1,'é',null];}
async function optional(flag:boolean):Promise<JsonValue|undefined>{
  return flag?parsed():undefined;
}
async function forward(pending:Promise<JsonValue>,trace:number[]):Promise<JsonValue>{
  try{return pending;}
  catch(e){trace.push(99);return null;}
  finally{trace.push(1);await ready(0);trace.push(2);}
}
export async function run(x:number):Promise<number>{
  const scalar:number=await liftNumber(-0);
  if(1/scalar!==-Infinity)throw new Error('lost signed zero');
  const value:Box={count:x};
  const packed=await liftRecord(value);
  const alias:Box=packed;
  if(alias===value || alias.count!==x+1)throw new Error('invalid native snapshot');
  alias.count=alias.count+1;
  if(packed.count!==x+1 || value.count!==x+1)throw new Error('shared native payload');
  const parsedValue:Box=await liftParsed();
  if(parsedValue.count!==3)throw new Error('lost parsed payload');
  if(JSON.stringify(await choose(false))!=='[1,"é",null]')throw new Error('lost contextual literal');
  const selected:number=await choose(true);
  if(selected!==7 || await optional(false)!==undefined)throw new Error('lost optional payload');
  const present:Box=await optional(true);
  if(present.count!==3)throw new Error('lost present payload');
  const trace:number[]=[];
  const error=new Error('original');
  let caught=false;
  try{await forward(reject(error),trace);}catch(e){caught=e===error;trace.push(3);}
  if(!caught || trace.length!==3)throw new Error('lost rejection scope');
  return trace[0]*100+trace[1]*10+trace[2];
}
"#,
    );
    for input in [0.0, 7.0, -4.0] {
        assert_eq!(program.throwing("run", input), (123.0, 0));
    }
}

#[test]
fn uncontended_settled_awaits_run_inline_without_per_await_storage() {
    let mut program = Program::new(
        r#"
async function ready():Promise<number>{return 7;}
async function consumer(trace:number[]):Promise<number>{
  trace.push(1);const value=await ready();trace.push(2);return value;
}
export async function order(n:number):Promise<number>{
  const trace:number[]=[];const result=consumer(trace);trace.push(3);await result;
  return trace[0]*100+trace[1]*10+trace[2];
}
export async function repeat(n:number):Promise<number>{
  const promise=ready();let sum=0;
  for(let i=0;i<n;i++)sum=sum+await promise;
  return sum;
}
"#,
    );
    assert_eq!(program.throwing("order", 0.0), (123.0, 0));
    assert_eq!(program.throwing("repeat", 1.0), (7.0, 0));
    let memory = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap();
    let before = memory.size(&program.store);
    assert_eq!(program.throwing("repeat", 100_000.0), (700_000.0, 0));
    assert_eq!(
        memory.size(&program.store),
        before,
        "settled awaits allocated queue nodes"
    );
}

#[test]
fn scaling_contracts_are_linear() {
    let workloads = [
        (
            "retained_records",
            r#"
interface Point { x: number; y: number }
export function run(x: number): number {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
    items.push({ x: i, y: i * 2 });
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    const point = items[k];
    sum = (sum + point.x + point.y) % 1000003;
  }
  return sum;
}
"#,
        ),
        (
            "number_push",
            r#"
export function run(x: number): number {
  const items: number[] = [];
  for (let i = 0; i < x; i++) {
    items.push(i * 3);
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    sum = (sum + items[k]) % 1000003;
  }
  return sum;
}
"#,
        ),
        (
            "temporary_records",
            r#"
interface Point { x: number; y: number }
function make(i: number): Point {
  return { x: i, y: i * 2 };
}
export function run(x: number): number {
  let sum = 0;
  for (let i = 0; i < x; i++) {
    const point = make(i);
    sum = (sum + point.x + point.y) % 1000003;
  }
  return sum;
}
"#,
        ),
    ];
    let expected = |x: u32| (0..x).fold(0u64, |sum, i| (sum + 3 * u64::from(i)) % 1000003) as f64;
    for (name, source) in workloads {
        let mut program = Program::new(source);
        let mut fuel = Vec::new();
        for n in [512u32, 512, 2048, 100_000] {
            let (results, used) = program.call("run", &[Val::F64(f64::from(n).to_bits())]);
            assert_eq!(results[0].unwrap_f64(), expected(n), "{name} n={n}");
            fuel.push(used);
        }
        let ratio = fuel[2] as f64 / fuel[1] as f64;
        let per_item = fuel[3] as f64 / 100_000.0;
        println!(
            "{name}: fuel n=512 {} 4n {} ratio {ratio:.2}, {per_item:.1}/item at 1e5",
            fuel[1], fuel[2]
        );
        assert!(ratio <= 6.0, "{name}: fuel ratio {ratio:.2} > 6 ({fuel:?})");
    }
}

#[test]
#[ignore]
fn dump() {
    let source = std::env::var("PERRY_DUMP").expect("PERRY_DUMP=<file.ts>");
    let wasm = build(&std::fs::read_to_string(source).unwrap());
    println!(
        "{} bytes\n{}",
        wasm.len(),
        wasmprinter::print_bytes(&wasm).unwrap()
    );
}

#[test]
fn catches_preserve_values_identity_and_implicit_failures() {
    let mut program = Program::new(
        r#"
function fail(value: Error): number { throw value; }
export function run(x: number): number {
  const original = new TypeError("bad " + "input");
  let score = 0;
  try { fail(original); } catch (e) {
    if (e === original && e instanceof Error && e instanceof TypeError && e.message === "bad input") score = score + 1;
    if (e !== new TypeError("bad input")) score = score + 2;
  }
  try { throw x; } catch (e) { if (e === x) score = score + 4; }
  try { throw "ab" + "cd"; } catch (e) { if (e === "abcd") score = score + 8; }
  try { throw true; } catch (e) { if (e === true) score = score + 16; }
  const values: number[] = [];
  try { values[2] = 5; } catch (e) { if (e instanceof RangeError) score = score + 32; }
  const words: string[] = [];
  try { score = score + words[0].length; } catch (e) { if (e instanceof TypeError) score = score + 64; }
  return score;
}
"#,
    );
    assert_eq!(program.number("run", 42.0), 127.0);
    assert_eq!(program.number("run", f64::NAN), 123.0);
}

#[test]
fn finally_preserves_or_overrides_each_completion() {
    let mut program = Program::new(
        r#"
function finish(x: number): number {
  try {
    if (x === 0) return 1;
    if (x === 1) throw new Error("first");
    return 3;
  } finally {
    if (x === 0) return 4;
    if (x === 1) throw new RangeError("last");
  }
}
export function run(x: number): number {
  let score = 0;
  try { score = finish(x); } catch (e) {
    if (e instanceof RangeError && e.message === "last") score = 9;
  }
  const original = new Error("original");
  try {
    try { throw original; } finally {
      try { throw new Error("temporary"); } catch (ignored) { score = score + 10; }
    }
  } catch (e) { if (e === original) score = score + 100; }
  for (let i = 0; i < 5; i++) {
    try {
      if (i === 0) continue;
      if (i === 2) break;
      score = score + 1000;
    } finally { score = score + 10000; }
  }
  return score;
}
"#,
    );
    for (x, result) in [(0.0, 31114.0), (1.0, 31119.0), (2.0, 31113.0)] {
        assert_eq!(program.number("run", x), result, "x={x}");
    }
}

#[test]
fn rejected_promises_retain_identity_across_repeated_awaits() {
    let mut program = Program::new(
        r#"
async function fail(error: Error): Promise<number> { throw error; }
export async function run(x: number): Promise<number> {
  const original = new Error("same");
  const pending = fail(original);
  let total = x;
  for (let i = 0; i < 3; i++) {
    try { total = total + await pending; } catch (e) {
      if (e === original) total = total + 1;
    } finally { total = total + 10; }
  }
  return total;
}

"#,
    );
    assert_eq!(program.throwing("run", 7.0), (40.0, 0));
}

#[test]
fn synchronous_raw_exports_do_not_discard_detached_rejections() {
    let mut program = Program::new(
        r#"
async function ready(): Promise<number> {return 0;}
async function fail(): Promise<void> {await ready(); throw new Error("detached");}
export function run(x: number): number {fail(); return x;}
"#,
    );
    let run = program
        .instance
        .get_typed_func::<f64, f64>(&mut program.store, "run")
        .unwrap();
    let error = run
        .call(&mut program.store, 7.0)
        .expect_err("detached rejection was discarded");
    assert!(matches!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::UnreachableCodeReached)
    ));
}

#[test]
fn byte_constructor_lengths_coerce_and_range_errors_are_catchable() {
    let mut program = Program::new(
        r#"
export function bytes(x: number): number {
  try { return new Uint8Array(x).length; }
  catch(e) { return e instanceof RangeError ? -1 : -2; }
}
export function buffer(x: number): number {
  try { return new ArrayBuffer(x).byteLength; }
  catch(e) { return e instanceof RangeError ? -1 : -2; }
}
"#,
    );
    for (input, expected) in [
        (-1.0, -1.0),
        (-0.5, 0.0),
        (-0.0, 0.0),
        (0.0, 0.0),
        (1.9, 1.0),
        (f64::NAN, 0.0),
        (f64::INFINITY, -1.0),
        (f64::NEG_INFINITY, -1.0),
        (4_294_967_296.0, -1.0),
    ] {
        for export in ["bytes", "buffer"] {
            assert_eq!(program.number(export, input), expected, "{export}({input})");
        }
    }
}

#[test]
fn oversized_byte_storage_traps_before_allocation_size_can_wrap() {
    for size in [4_294_967_295.0f64, 4_294_967_288.0] {
        let mut program = Program::new(
            "export function run(n: number): number { return new Uint8Array(n).length; }",
        );
        let run = program
            .instance
            .get_func(&mut program.store, "run")
            .unwrap();
        let mut results = [Val::F64(0), Val::I32(0)];
        let error = run
            .call(
                &mut program.store,
                &[Val::F64(size.to_bits())],
                &mut results,
            )
            .expect_err("allocator must reject alignment/header overflow");
        assert!(
            matches!(
                error.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::UnreachableCodeReached)
            ),
            "{error:#}"
        );
    }
}

#[test]
fn json_failures_are_caught_and_narrowing_checks_real_variants() {
    let mut program = Program::new(
        r#"
function number(value: number): number { return value + 1; }
function text(value: string): number { return value.length; }
export function run(x: number): number {
  let score = 0;
  try { JSON.parse('{bad'); } catch(e) {
    if(e instanceof SyntaxError && e instanceof Error) score = score + 1;
    const original = e;
    try { throw e; } catch(second) { if(second === original) score = score + 2; }
  }
  const parsed = JSON.parse('{"number":2,"text":"é🙂","null":null,"list":[1,2]}');
  score = score + number(parsed.number) + text(parsed.text);
  try { number(parsed.text); } catch(e) { if(e instanceof TypeError) score = score + 8; }
  try { text(parsed.number); } catch(e) { if(e instanceof TypeError) score = score + 16; }
  try { number(parsed.missing); } catch(e) { if(e instanceof TypeError) score = score + 32; }
  try { parsed.null.foo; } catch(e) { if(e instanceof TypeError) score = score + 64; }
  if(parsed.missing === undefined && parsed.null === null) score = score + 128;
  if(JSON.stringify(parsed.missing) !== undefined || JSON.stringify(undefined) !== undefined) throw 2;
  const nested: number[][] = [[1]];
  if(!Array.isArray(nested[0]) || Array.isArray(nested[1])) throw 3;
  const array = parsed.list;
  array[2] = x;
  if(array.length === 3 && array[2] === x && array === parsed.list) score = score + 256;
  try { array[4] = 8; } catch(e) { if(e instanceof RangeError) score = score + 512; }
  array[0] = array;
  try { JSON.stringify(array); } catch(e) { if(e instanceof TypeError) score = score + 1024; }
  return score;
}
"#,
    );
    assert_eq!(program.number("run", 9.0), 2048.0);
}
