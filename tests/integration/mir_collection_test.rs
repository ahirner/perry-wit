//! Collector behaviour of compiled core modules: reclamation, forced collection with
//! poisoning, retained roots across suspension, and allocation and root-publication counts.
use crate::core_program::{Counter, Program};
use crate::node_oracle::NodeOracle;

#[test]
fn concat_and_spread_match_node_for_order_nullability_and_shallow_identity() -> anyhow::Result<()> {
    let source = r#"
interface Cell { value:number }
interface Holder { items:Cell[] }
function mutate(items:Cell[]):number { items[0].value=9;return 3; }
function mutateNumbers(a:number[]):number[] { a[0]=9;return [3]; }
export function run():number {
 const a:number[]=[1,2];
 const alias=a;
 const spread=[...a,...mutateNumbers(a),10];
 if(spread[0]!==1 || spread[2]!==3 || alias[0]!==9)return 9;
 const combined=a.concat(mutateNumbers(a),4,...[5,6]);
 if(combined===a || combined[0]!==9 || combined[2]!==3 || combined[5]!==6)return 10;
 combined[0]=7;
 const copiedNumbers=a.concat();
 copiedNumbers[0]=8;
 if(a[0]!==9 || spread.length+combined.length+copiedNumbers.length!==12)return 12;
 const cell:Cell={value:1};const values:Cell[]=[cell];
 const copied=[...values];copied[0].value=2;
 if(values[0]!==cell || cell.value!==2)return 1;
 const nested:number[][]=[[1],[2]];
 const flat=nested.concat([[3]]);
 if(flat.length!==3 || flat[0]!==nested[0])return 2;
 const text:string[]=['é','🙂'];
 const emptyMix=[...[],...text];const emptyCopy=values.concat(...[]);
 if(emptyMix.length!==2 || emptyCopy===values || ''.concat(...[])!=='')return 11;
 const nullable=[null,...text,undefined];
 if(nullable[0]!==null || nullable[1]!=='é' || nullable[3]!==undefined)return 3;
 const optional:(number|null)[]=[1,null];
 const joined=optional.concat(null,[2]);
 if(joined.length!==4 || joined[2]!==null)return 4;
 const repeated=values.concat(values);
 repeated[0].value=7;
 if(repeated[1]!==cell || values[0].value!==7)return 5;
 const empty:number[]=[];empty.push(...empty);
 const self:number[]=[1,2];self.push(...self);
 if(self.length!==4 || self[2]!==1 || self[3]!==2)return 6;
 const words='start'.concat(...text,mutate(values));
 if(words!=='starté🙂3' || cell.value!==9)return 7;
 const holder:Holder={items:values};
 const escape=holder.items.concat([{value:4}]);escape[0]={value:8};
 if(holder.items[0]!==cell || holder.items.length!==1)return 8;
 return 0;
}
"#;
    let oracle = NodeOracle::typescript(source, "console.log(run());")?;
    let expected = oracle.run(&[])?.trim().parse::<f64>()?;
    let mut program = Program::checked(source, "concat.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (expected, 0));
    Ok(())
}

#[test]
fn owned_left_right_and_both_arrays_reuse_capacity_across_joins() {
    for expression in [
        "owned.concat(borrowed)",
        "borrowed.concat(owned)",
        "owned.concat([2,3])",
    ] {
        let source = format!(
            "export function run(n:number):number{{const borrowed:number[]=[2,3];let total=0;for(let i=0;i<n;i++){{const owned:number[]=[1];const joined={expression};total+=joined.length+joined[0];}}return total;}}"
        );
        let mut program = Program::checked(&source, "concat-capacity.ts");
        program.stress_collection(true);
        let expected = if expression.starts_with("borrowed") {
            5000.0
        } else {
            4000.0
        };
        assert_eq!(program.number("run", 1000.0), expected);
        assert!(
            program.counter(Counter::Allocations)
                < if expression.ends_with("[2,3])") {
                    4100
                } else {
                    2100
                },
            "{expression}: {} allocations",
            program.counter(Counter::Allocations)
        );
    }
    let source = r#"
interface Cell { value: number }
export function run(n: number): number {
  let numbers: number[] = [];
  let cells: Cell[] = [];
  for (let i = 0; i < n; i++) {
    if (i % 2 === 0) numbers = [i].concat(numbers);
    else numbers = numbers.concat([i]);
    cells = [...cells, { value: i }];
  }
  return numbers.length + numbers[0] + cells.length + cells[0].value + cells[cells.length - 1].value;
}
"#;
    let mut program = Program::checked(source, "concat-join.ts");
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 1000.0), (3997.0, 0));
    assert!(program.counter(Counter::Allocations) < 5400);
}

#[test]
fn unchanged_module_graph_does_not_collect_on_every_export() {
    let source = r#"
interface Cell { value: number; next: Cell | null }
function build(): Cell {
  let head: Cell = { value: 0, next: null };
  for (let i = 1; i < 5000; i++) head = { value: i, next: head };
  return head;
}
const saved: Cell = build();
export function run(n: number): number { return saved.value + n; }
"#;
    let mut program = Program::checked(source, "retained.ts");
    assert_eq!(program.number("run", 1.0), 5000.0);
    let before = program.counter(Counter::Collections);
    for i in 0..100 {
        assert_eq!(program.number("run", f64::from(i)), 4999.0 + f64::from(i));
    }
    assert_eq!(program.counter(Counter::Collections), before);
}

#[test]
fn collection_reclaims_overwrites_and_preserves_reachable_cycles() {
    let mut program = Program::new(
        r#"
interface Node { value: number; next: Node | null }
export function run(n: number): number {
  const first: Node = { value: 7, next: null };
  const second: Node = { value: 11, next: first };
  first.next = second;
  const latest: Node[] = [first];
  for (let i = 0; i < n; i++) latest[0] = { value: i, next: null };
  const next = first.next;
  if (next === null) return -1;
  if (next.next !== first) return -2;
  return first.value + next.value + latest[0].value;
}
"#,
    );
    assert_eq!(program.throwing("run", 200_000.0), (200_017.0, 0));
    assert!(
        program.memory_bytes() <= 512 * 1024,
        "{} bytes",
        program.memory_bytes()
    );
}

#[test]
fn completed_async_frames_and_promises_are_collectible() {
    let mut program = Program::new(
        r#"
interface Cell { value: number; next: Cell | null }
async function advance(cell: Cell): Promise<Cell> {
  const pending = Promise.resolve(cell);
  const first = await pending;
  const again = await pending;
  return { value: first.value + again.value, next: null };
}
export async function run(n: number): Promise<number> {
  let current: Cell = { value: 1, next: null };
  for (let i = 0; i < n; i++) {
    const cell: Cell = { value: i, next: current };
    current = await advance(cell);
  }
  return current.value;
}
"#,
    );
    assert_eq!(program.throwing("run", 200_000.0), (399_998.0, 0));
    assert!(
        program.memory_bytes() <= 512 * 1024,
        "completed async objects accumulated: {} bytes",
        program.memory_bytes()
    );
}

#[test]
fn builtin_churn_preserves_shared_views_and_reclaims_temporaries() {
    let mut program = Program::new(
        r#"
export function run(n: number): number {
  const initial = new TextEncoder().encode('é🙂!');
  const retained = initial.subarray(2, 6);
  const prefix = ('prefix-' + n).slice(0, 6);
  const parsed = JSON.parse('{"keep":{"value":17},"drop":[1,2,3]}').keep;
  let total = 0;
  for (let i = 0; i < n; i++) {
    const bytes = new Uint8Array([i, 7, 8]);
    const tail = bytes.subarray(1);
    tail[0] = 9;
    const text = ('é🙂-' + i).slice(1, 3);
    total = (total + text.length + bytes[1]) % 100003;
    const json = JSON.parse('{"value":5}');
    total = (total + json.value) % 100003;
  }
  return total + retained.length + prefix.length + parsed.value;
}
"#,
    );
    assert_eq!(
        program.throwing("run", 200_000.0),
        ((200_000.0 * 16.0) % 100003.0 + 27.0, 0)
    );
    let warmed = program.memory_bytes();
    assert!(
        warmed <= 1024 * 1024,
        "builtin churn retained {warmed} bytes"
    );
    program.throwing("run", 400_000.0);
    assert_eq!(
        program.memory_bytes(),
        warmed,
        "builtin memory did not stabilize"
    );
}

#[test]
fn forced_collection_poisoning_preserves_suspended_and_persistent_roots() {
    let source = r#"
interface Cell { value: number; next: Cell | null }
let saved: Cell = { value: 7, next: null };
saved.next = saved;
async function compute(n: number): Promise<number> {
    const bytes = new TextEncoder().encode('é🙂');
    const alias = bytes.subarray(2);
    const payload = JSON.parse('{"value":11}');
    const cell: Cell = { value: n, next: saved };
    const settled = await Promise.all([Promise.resolve(n), Promise.resolve(n + 1)]);
    for (let i = 0; i < n; i++) {
        const temporary: Cell = { value: i, next: null };
        saved.value = temporary.value;
        const text = 'iteration:' + i;
        if (text.length < 2) throw new Error('bad text');
    }
    return cell.value + cell.next.value + alias[0] + payload.value + settled[1];
}
export async function run(n: number): Promise<number> { return await compute(n); }
"#;
    let mut program = Program::checked(source, "stress.ts");
    program.stress_collection(true);
    for n in [40.0, 80.0, 120.0] {
        assert_eq!(program.throwing("run", n), (3.0 * n + 251.0, 0));
        for counter in [Counter::Pins, Counter::Roots] {
            assert_eq!(
                program.counter(counter),
                0,
                "completed task retained {counter:?}"
            );
        }
    }
    assert!(
        program.counter(Counter::Collections) > 240,
        "safe points did not force collection"
    );
    assert!(
        program.counter(Counter::LiveBytes) < 128,
        "only module state should survive"
    );
}

#[test]
fn proven_frame_records_reuse_storage_across_calls_and_iterations() {
    let source = r#"
interface Cell { value: number }
export function inspect(cell: Cell): number {
    if (cell.value < 0) throw new RangeError('negative');
    let result = 0;
    for (let i = 0; i < 3; i++) result = result + cell.value;
    return result;
}
export function run(n: number): number {
    let result = 0;
    for (let i = 0; i < n; i++) {
        const cell: Cell = { value: i };
        result = result + inspect(cell);
    }
    return result;
}
"#;
    let mut program = Program::checked(source, "frames.ts");
    program.stress_collection(true);
    assert_eq!(program.number("run", 40.0), 2340.0);
    program.stress_collection(false);
    assert_eq!(
        program.number("run", 200_000.0),
        3.0 * 200_000.0 * 199_999.0 / 2.0
    );
    assert_eq!(
        program.counter(Counter::Allocations),
        2,
        "one frame per activation, independent of iterations"
    );
    assert_eq!(program.counter(Counter::LiveBytes), 0);
    assert!(program.memory_bytes() <= 512 * 1024);
}

#[test]
fn mutable_dates_survive_calls_suspension_and_collection() {
    let source = r#"
let saved: Date = new Date(0);
function set(date: Date, time: number): number { return date.setTime(time); }
export async function run(n: number): Promise<number> {
    const alias = saved;
    const dates: Date[] = [saved];
    for (let i = 0; i < n; i++) {
        const temporary = new Date(i);
        set(temporary, i + 1);
        saved = temporary;
    }
    const result = await Promise.resolve(saved.getTime());
    set(alias, 7);
    return result + dates[0].getTime();
}
"#;
    let mut program = Program::checked(source, "dates.ts");
    let initial = program.memory_bytes();
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 120.0), (127.0, 0));
    program.stress_collection(false);
    assert_eq!(program.throwing("run", 200_000.0), (200_007.0, 0));
    let size = program.memory_bytes();
    let live = program.counter(Counter::LiveBytes);
    assert!(
        size <= 512 * 1024,
        "initial={initial} size={size} live={live:?}"
    );
}

#[test]
fn temporal_values_survive_collection_and_reclaim_temporary_storage() {
    let source = r#"
interface CalendarValues { date: Temporal.PlainDate; time: Temporal.PlainDateTime; }
let saved: Temporal.Instant = Temporal.Instant.fromEpochMilliseconds(-1);
export async function run(n: number): Promise<number> {
    const original = saved;
    const values: CalendarValues = { date: Temporal.PlainDate.from('2024-02-28'),
        time: Temporal.PlainDateTime.from('2024-02-28T12:30:00.123456789') };
    const dates: Temporal.PlainDate[] = [values.date];
    for (let i = 0; i < n; i++) {
        saved = Temporal.Instant.fromEpochMilliseconds(i);
        const next = values.date.add({ days: 1 });
        if (next.day !== 29 || next.toString() !== '2024-02-29') throw 1;
        if (saved.toString().length === 0) throw 2;
    }
    const settled = await Promise.resolve(saved);
    if (original.toString().length === 0 || dates[0].day !== 28) throw 3;
    if (values.time.nanosecond !== 789) throw 4;
    return settled.epochMilliseconds;
}
"#;
    let mut program = Program::checked(source, "temporal.ts");
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 120.0), (119.0, 0));
    program.stress_collection(false);
    for _ in 0..2 {
        assert_eq!(program.throwing("run", 200_000.0), (199_999.0, 0));
        assert!(program.memory_bytes() <= 512 * 1024);
    }
}

#[test]
fn frame_storage_is_released_on_exception_and_finalizer_paths() {
    let source = r#"
interface Cell { value: number }
function inspect(cell: Cell): number {
    let result = 0;
    for (let i = 0; i < 3; i++) result = result + cell.value;
    return result;
}
function fail(n: number): number {
    const cell: Cell = { value: n };
    try { if (n > 0) throw n; return inspect(cell); }
    finally { if (inspect(cell) < 0) throw 77; }
}
export function run(n: number): number {
    let total = 0;
    for (let i = 0; i < n; i++) {
        try { total = total + fail(i); } catch (error) { total = total + 1; }
    }
    return total;
}
"#;
    let mut program = Program::checked(source, "frame-throws.ts");
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 120.0), (119.0, 0));
    for counter in [Counter::LiveBytes, Counter::Pins, Counter::Roots] {
        assert_eq!(
            program.counter(counter),
            0,
            "completed task retained {counter:?}"
        );
    }
}

#[test]
fn incremental_decoder_copies_only_pending_bytes_and_survives_collection() {
    let source = r#"
export async function run(n: number): Promise<number> {
    const decoder = new TextDecoder('utf-8', { fatal: true });
    const alias = decoder;
    const complete = new Uint8Array([65, 0xc3, 0xa9, 0xf0, 0x9f, 0x99, 0x82]);
    const stable = decoder.decode(complete);
    complete.fill(66);
    const head = new Uint8Array([0xf0, 0x9f]);
    const before = decoder.decode(head, { stream: true });
    head.fill(65);
    for (let i = 0; i < n; i++) {
        const garbage = 'temporary:' + i;
        if (garbage.length < 5) throw new Error('bad string');
    }
    await Promise.resolve(n);
    const result = alias.decode(new Uint8Array([0x99, 0x82]));
    return before.length + result.length + result.codePointAt(0) + (stable === 'Aé🙂' ? 7 : -1);
}
"#;
    let mut program = Program::checked(source, "decoder.ts");
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 120.0), (128586.0, 0));
    for counter in [Counter::LiveBytes, Counter::Pins, Counter::Roots] {
        assert_eq!(
            program.counter(counter),
            0,
            "completed decoder task retained {counter:?}"
        );
    }
}

#[test]
fn finite_variant_graphs_survive_forced_collection_and_checked_json() {
    let source = r#"
type Value = { tag: 'empty' } | { tag: 'number', val: number } | { tag: 'text', val: string } | { tag: 'point', val: { x: number, label: string } };
let retained: Value = { tag: 'point', val: { x: 9, label: 'retained' } };
function make(n: number): Value {
    if (n % 3 === 0) return { tag: 'number', val: n };
    if (n % 3 === 1) return { tag: 'text', val: 'value:' + n };
    return { tag: 'point', val: { x: n, label: 'point:' + n } };
}
export function run(n: number): number {
    let value: Value = { tag: 'empty' };
    for (let i = 0; i < n; i++) value = make(i);
    const parsed: Value = JSON.parse('{"tag":"point","val":{"x":7,"label":"json"}}');
    let total = 0;
    if (retained.tag === 'point') total = retained.val.x + retained.val.label.length;
    if (parsed.tag === 'point') total = total + parsed.val.x;
    if (value.tag === 'number') total = total + value.val;
    if (value.tag === 'text') total = total + value.val.length;
    if (value.tag === 'point') total = total + value.val.x;
    if (JSON.stringify(parsed) !== '{"tag":"point","val":{"x":7,"label":"json"}}') throw new Error('serialization');
    try {
        const bad: Value = JSON.parse('{"tag":"number","val":null}');
        return -1;
    } catch (error) { if (!(error instanceof TypeError)) return -2; }
    try {
        const bad: Value = JSON.parse('{"tag":"unknown"}');
        return -3;
    } catch (error) { if (!(error instanceof TypeError)) return -4; }
    return total;
}
"#;
    let mut program = Program::checked(source, "variants.ts");
    program.stress_collection(true);
    for n in [3000.0, 3001.0, 3002.0] {
        let expected = 24.0 + if n == 3002.0 { 10.0 } else { n - 1.0 };
        assert_eq!(program.throwing("run", n), (expected, 0));
    }
    assert!(
        program.counter(Counter::LiveBytes) < 4096,
        "discarded variant payloads remained live"
    );
}

#[test]
fn closed_number_string_values_preserve_tags_equality_and_checked_json() {
    let source = r#"
interface Item { value?: number | string | null; }
function text(value: number | string): string {
    if (typeof value === 'string') return value;
    return JSON.stringify(value);
}
function size(value: number | string): number {
    if (typeof value === 'number') { return value + 1; }
    else { return value.length; }
}
function same(a: number | string, b: number | string): boolean { return a === b; }
let retained: Item = {};
export function run(n: number): number {
    const absent: Item = {};
    const nil: Item = {value: null};
    if (typeof absent.value !== 'undefined' || typeof nil.value !== 'object') return -1;
    const values: (number | string)[] = [n, 'é😀'];
    if (text(values[0]) !== '7' || text(values[1]) !== 'é😀') return -2;
    if (!same('abc', 'a' + 'bc') || same(7, '7') || same(NaN, NaN) || !same(0, -0)) return -3;
    const parsed: Item = JSON.parse('{"value":"ok"}');
    if (typeof parsed.value !== 'string') return -4;
    if (JSON.stringify(parsed) !== '{"value":"ok"}') return -5;
    if (JSON.stringify(values) !== '[7,"é😀"]') return -6;
    if (String(values[0]) !== '7' || String(values[1]) !== 'é😀') return -7;
    if (values[0] !== 7 || values[1] !== 'é😀') return -8;
    let sum = 0;
    for (let i = 0; i < 3000; i++) {
        const item: Item = {value: i};
        retained = item;
        if (typeof item.value === 'number') sum += item.value;
    }
    if (retained.value !== 2999) return -9;
    try { const bad: Item = JSON.parse('{"value":true}'); return -10; }
    catch (error) { if (!(error instanceof TypeError)) return -11; }
    return size(values[0]) + size(values[1]) + sum;
}
"#;
    let mut program = Program::checked(source, "number-string.ts");
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 7.0), (4_498_510.0, 0));
    assert!(program.counter(Counter::LiveBytes) < 4096);
}

#[test]
fn allocation_free_calls_do_not_collect_or_publish_roots() {
    let mut program = Program::checked(
        r#"
function leaf(n: number): number { if (n < 0) return -n; return n + 1; }
function step(n: number): number { return leaf(n) + 2; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) sum += step(i);
  return sum;
}
"#,
        "no-gc-calls.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 502_500.0);
    for (counter, expected) in [
        (Counter::Allocations, 0),
        (Counter::Collections, 1),
        (Counter::Roots, 0),
        (Counter::RootPublications, 0),
    ] {
        assert_eq!(program.counter(counter), expected, "{counter:?}");
    }
}

#[test]
fn nested_root_stack_growth_preserves_caller_objects_and_unwinds() {
    let mut program = Program::checked(
        r#"
interface Cell { value: number }
export function read(cell: Cell): number { if (cell.value < 0) throw new RangeError('negative'); return cell.value; }
function descend(n: number, fail: boolean): number {
  const cell = { value: n };
  if (n === 0) { if (fail) throw new RangeError("bottom"); return read(cell); }
  try { return descend(n - 1, fail) + read(cell); }
  catch (e) { return read(cell) + 1000; }
}
export function run(n: number): number {
  const a = descend(n, false);
  const b = descend(n, true);
  return a + b;
}
"#,
        "root-stack.ts",
    );
    program.stress_collection(true);
    for n in [64.0, 120.0, 64.0] {
        assert_eq!(program.number("run", n), n * (n + 1.0) + 1000.0);
        for counter in [Counter::LiveBytes, Counter::Pins, Counter::Roots] {
            assert_eq!(program.counter(counter), 0, "retained {counter:?}");
        }
    }
}

#[test]
fn non_error_messages_remain_collectible() {
    let mut program = Program::checked(
        r#"
export function run(n: number): number {
  let sum = 0;
  try { throw 0; } catch (error) {
    for (let i = 0; i < n; i++) sum += error.message.length;
  }
  return sum;
}
"#,
        "non-error-message.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 1000.0), (0.0, 0));
    program.stress_collection(false);
    assert_eq!(program.throwing("run", 200_000.0), (0.0, 0));
    assert!(program.memory_bytes() <= 512 * 1024);
    for counter in [Counter::LiveBytes, Counter::Roots] {
        assert_eq!(program.counter(counter), 0, "retained {counter:?}");
    }
}

#[test]
fn compilation_preserves_root_and_async_frame_order() {
    let source = r#"
function trimmed(s:string):string { return s.trim(); }
function upper(s:string):string { return s.toUpperCase(); }
function lower(s:string):string { return s.toLowerCase(); }
export async function run(n:number):Promise<number> {
    const a = trimmed(" A" + n + " ");
    const b = upper("b" + n);
    const c = lower("C" + n);
    const d = await Promise.resolve(a + b);
    return a.length + b.length + c.length + d.length + 3;
}
"#;
    let compile = || {
        perry_wit::compiler::compile_core_with_memory_checks(source, "root-order.ts")
            .expect("source compiles")
    };
    let wasm = compile();
    for _ in 0..16 {
        assert!(
            wasm == compile(),
            "identical source must emit identical Wasm"
        );
    }
    let mut program = Program::from_wasm(wasm);
    program.stress_collection(true);
    assert_eq!(program.number("run", 1.0), 13.0);
}

#[test]
fn nonthrowing_intrinsics_allow_temporary_record_elimination() {
    let mut program = Program::checked(
        r#"
        interface Cell { value: number }
        function rounded(n:number):Cell { return { value:Math.floor(n) }; }
        export function run(n:number):number {
            let sum = 0;
            for(let i=0;i<n;i++) sum += rounded(i - 0.25).value;
            return sum;
        }
        "#,
        "intrinsic-leaf.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 498_500.0);
    assert_eq!(program.counter(Counter::Allocations), 0);
}

#[test]
fn promoted_record_arguments_stay_fresh_when_aliases_can_mutate() {
    let mut program = Program::checked(
        r#"
interface Cell { value: number }
let saved: Cell = { value: 0 };
function change(): number { saved.value += 10; return 3; }
function read(cell: Cell, n: number): number {
  if (n < 0) return cell.value - n;
  return cell.value + n;
}
function mutated(cell: Cell): number {
  const before = cell.value;
  const middle = change();
  return before + middle + cell.value;
}
function escaped(cell: Cell): Cell { saved = cell; return cell; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const cell: Cell = { value: i };
    // A call in a caught region stays a call: promotion can still remove the record.
    try { sum += read(cell, i); } catch (e) { return -1; }
  }
  saved = escaped({ value: 7 });
  return sum + mutated(saved);
}
"#,
        "promoted-record.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 999_027.0);
    assert!(program.counter(Counter::Allocations) < 10);
}

#[test]
fn atomic_helpers_publish_roots_only_when_collection_is_due() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
export function run(n: number): number {
  let text = '';
  let prefix = '';
  for (let i = 0; i < n; i++) {
    if (i === 10) prefix = text;
    text += 'é😀';
  }
  if (prefix !== 'é😀'.repeat(10) || text.charAt(1) !== '😀') return -1;
  return text.length + prefix.length;
}
"#,
        "atomic-roots.ts",
    )
    .expect("source compiles");
    for stress in [false, true] {
        let mut program = Program::from_wasm(wasm.clone());
        program.stress_collection(stress);
        assert_eq!(program.number("run", 30.0), 80.0);
        let published = program.counter(Counter::RootPublications);
        if stress {
            assert!(
                published > 30,
                "forced safe points must retain roots: {published}"
            );
        } else {
            assert!(published < 20, "unnecessary root publications: {published}");
        }
        assert_eq!(program.counter(Counter::Roots), 0);
    }
}

#[test]
fn in_module_awaits_preserve_cyclic_loop_values_under_forced_collection() {
    let source = r#"
async function step(n: number): Promise<number> { return await Promise.resolve(n + 1); }
export async function run(n: number): Promise<number> {
  let a = 1; let b = 2; let c = 3;
  const marker = 'é' + String(n) + '😀';
  for (let i = 0; i < n; i++) {
    const old = a; a = b; b = c; c = await step(old + i);
    if (marker[0] !== 'é' || marker[marker.length - 1] !== '😀') throw 1;
  }
  return a * 10000 + b * 100 + c;
}
"#;
    let mut program = Program::checked(source, "async-dispatch.ts");
    program.stress_collection(true);
    for n in [0, 1, 2, 3, 19, 100] {
        let (mut a, mut b, mut c) = (1, 2, 3);
        for i in 0..n {
            (a, b, c) = (b, c, a + i + 1);
        }
        assert_eq!(
            program.throwing("run", f64::from(n)),
            (f64::from(a * 10000 + b * 100 + c), 0)
        );
    }
}

/// Abandoned iterators and insert/delete churn reuse memory; fuel scaling is the
/// `wasi.execution.scaling.collection_churn` conformance contract.
#[test]
fn collection_churn_reuses_memory() {
    let mut churn = Program::new(
        r#"
export function run(n: number): number {
    const s = new Set<number>([0]);
    const m = new Map<number, number>([[0, 0]]);
    for (let i = 0; i < n; i = i + 1) {
        s.values();
        m.values();
        for (const _v of s.values()) break;
        for (const _v of s) break;
        for (const _e of m) break;
        s.add(i + 1);
        s.delete(i + 1);
        m.set(i + 1, i);
        m.delete(i + 1);
    }
    return s.size + m.size;
}
"#,
    );
    assert_eq!(churn.number("run", 128.0), 2.0);
    let warmed = churn.memory_bytes();
    for n in [512.0, 65536.0] {
        assert_eq!(churn.number("run", n), 2.0);
        assert_eq!(churn.memory_bytes(), warmed, "churn of {n} grew memory");
    }
    assert!(warmed <= 65536, "expected one 64 KiB page, got {warmed}");
}

/// Inline and named callbacks see `(item, index, array)` over the original length, propagate
/// exceptions, run `finally`, return through `switch` and endless loops (an empty return is
/// `undefined`), and `for...of` visits elements pushed during iteration and string scalars;
/// all under forced collection with poisoning.
#[test]
fn array_callbacks_and_iteration_survive_forced_collection() {
    let source = r#"
function twice(n:number):number {return n*2;}
function nothing(n:number):void {}
function missingDefault():number {throw new Error('missing');}
function refill(values:number[]):number {values.push(1,2);return 8;}
export function callbacks():number {
 let calls=0;
 const numbers:number[]=[1,2,3];
 const mapped=numbers.map((n,i,a)=>{calls++;if(i===0)a.push(4);return n+i;});
 const doubled=mapped.filter(n=>n>1).map(twice);
 if(calls!==3 || mapped.length!==3 || doubled[0]!==6 || doubled[1]!==10)return 1;
 const values:number[]=[1,2];
 let trace=0;
 try {values.map((n,i,a)=>{trace=trace*10+n;if(i===1)throw new Error('stop');a[1]=4;return n;});return 2;}
 catch(e){if(e.message!=='stop')return 3;}
 const finished=values.map(n=>{try {return n*2;}finally {trace++;}});
 if(trace!==16 || finished[1]!==8)return 4;
 const wrapped=values.map(n=>n+(trace as number));
 if(wrapped[0]!==17 || wrapped[1]!==20)return 7;
 const small:number[]=[1,2,3];
 const cases:number[]=small.map(n=>{switch(n){case 1:return 7;case 2:default:return n;}});
 const loops:number[]=small.map(n=>{while(true){return n*2;}});
 const empty=small.map(n=>{switch(n){case 1:return;default:return n;}});
 const overridden=small.map(n=>{try{return;}finally{return n;}});
 const ignored=small.map(n=>{return n;return;});
 if(cases[0]+cases[1]+cases[2]+loops[2]!==18 || empty[0]!==undefined || empty[1]!==2 || overridden[0]!==1 || ignored[0]!==1)return 5;
 const optional=small.map(n=>{if(n===1)return 'yes';});
 const bare=small.map(n=>{if(n===1)return;return 'value';});
 if(optional[0]!=='yes' || optional[1]!==undefined || bare[0]!==undefined || bare[1]!=='value')return 6;
 const nested=numbers.map(n=>{const adjust=(v:number)=>v+n;const inner=small.map(adjust);return inner[0]+adjust(1);});
 if(nested[0]!==4 || nested[3]!==10 || small[0]!==1)return 8;
 return small.filter(nothing).length;
}
export function iteration():number {
 const values:number[]=[1,2];
 let visits=0;
 for(const n of values){visits++;if(n===1){values.push(3);continue;}if(n===3)break;}
 let text='';
 for(const word of 'c,d'.split(',')){text+=word;}
 const [first, , missing = visits + 4] = values;
 const [label, count] = ['ok', 3] as [string, number];
 if(first!==1 || missing!==3 || label!=='ok' || count!==3)return 2;
 const [, absent = 7] = [5];
 if(absent!==7)return 3;
 let thrown=0;
 try {const [value = missingDefault()] = []; } catch(e) {thrown++;}
 if(thrown!==1)return 4;
 const exhausted:number[]=[];
 const [filled=refill(exhausted), stillMissing=9]=exhausted;
 if(filled!==8 || stillMissing!==9)return 5;
 for(const scalar of 'é🙂a'){text+=scalar+':';}
 return visits===3 && text==='cdé:🙂:a:' ? 0 : 1;
}
"#;
    let mut program = Program::checked(source, "array-callbacks.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("callbacks", &[]), (0.0, 0));
    assert_eq!(program.call("iteration", &[]).0[0].unwrap_f64(), 0.0);
}

/// Dynamic JSON lookups through computed keys, spread copies and scalar string indexing release
/// everything they allocate, under forced collection.
#[test]
fn json_pointer_lookup_releases_its_storage() {
    let source = r#"
interface Options { extra?: Array<[string, { jsonText: string }]> }
function length(options: Options): number {
    const extra = options.extra;
    if (extra === undefined) return -1;
    return extra[0][0].length + extra[0][1].jsonText.length;
}
export function run(): number {
    const raw = JSON.parse('{"2":2,"0":1,"a":[{"":9}],"0":3}');
    const keys = Object.keys(raw);
    if (keys[0] !== '0' || keys[1] !== '2' || keys[2] !== 'a') return -2;
    const a = raw[keys[2]];
    const first = a['0'];
    const encoded = JSON.stringify(raw);
    if (encoded !== '{"0":3,"2":2,"a":[{"":9}]}') return -6;
    if (JSON.stringify({ ...raw }) !== encoded) return -7;
    if (a['00'] !== undefined) return -3;
    const text = 'é🙂';
    if (text[-1] !== undefined || text[2] !== undefined || text[0.5] !== undefined) return -4;
    if (text[1] !== '🙂') return -5;
    return raw[0] + first[''] + a['length'] + length({extra: [['status', {jsonText: 'ok'}]]});
}
"#;
    let mut program = Program::checked(source, "json-pointer-lookup.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        assert_eq!(program.outcome("run", &[]), (21.0, 0));
        assert_eq!(program.counter(Counter::LiveBytes), 0);
    }
}
