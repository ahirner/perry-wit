//! Regressions from consumer programs: checked JSON shapes, narrowing, truthiness and type
//! resolution across modules.
use anyhow::Result;

use crate::core_program::{Counter, Program};

/// Inline and named callbacks see `(item, index, array)` over the original length, propagate
/// exceptions, run `finally`, return through `switch` and endless loops (an empty return is
/// `undefined`), and `for...of` visits elements pushed during iteration and string scalars.
#[test]
fn array_callbacks_and_iteration_match_javascript() {
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

/// `includes` (SameValueZero) and `indexOf` (strict equality) over numbers, optionals, mixed
/// values and records with ECMAScript start positions; `push` evaluates every argument and
/// spread before the receiver grows, so a failed argument pushes nothing.
#[test]
fn array_search_and_push_match_javascript() {
    let source = r#"
interface Item {n:number}
const values:number[]=[1];
function start():number {values.push(2);return 0;}
const a:number[]=[1,2];
const rest:number[]=[3,4];
function later():number {rest[0]=8;return a.length;}
function fail():number {throw new Error('argument');}
function failText():string {throw new Error('argument');}
export function search():number {
 if(!values.includes(2,start()))return 1;
 const item:Item={n:1};
 const other:Item={n:1};
 const objects:Item[]=[item];
 if(!objects.includes(item) || objects.includes(other) || objects.indexOf(other)!==-1)return 2;
 const words:string[]=['a','b'];
 if(!words.includes('b') || words.indexOf('b',-1)!==1)return 3;
 const numbers:number[]=[1,4];
 if(!numbers.includes(1,NaN) || numbers.includes(1,Infinity) || !numbers.includes(1,-Infinity))return 4;
 if(numbers.indexOf(4,-2.8)!==1 || numbers.indexOf(1,0.8)!==0 || numbers.includes(1,-1))return 5;
 const special:number[]=[NaN,0];
 if(!special.includes(NaN) || special.indexOf(NaN)!==-1 || !special.includes(-0))return 6;
 const optional:(number|undefined)[]=[undefined,NaN,1];
 if(!optional.includes(NaN) || !optional.includes(undefined) || optional.indexOf(NaN)!==-1)return 7;
 const mixed:(string|number)[]=['x',NaN];
 return mixed.includes(NaN) && mixed.indexOf(NaN)===-1 ? 0 : 8;
}
export function push():number {
 const pushed:number[]=[];
 if(pushed.push(1,pushed.length)!==2 || pushed[1]!==0 || pushed.push()!==2)return 1;
 const holder:{items:string[]}={items:[]};
 try {holder.items.push('first',failText());}catch(e){}
 if(holder.items.length!==0)return 2;
 if(a.push(0,...rest,later())!==6)return 3;
 if(a[3]!==3 || a[4]!==4 || a[5]!==2)return 4;
 if(a.push(...a)!==12 || a[9]!==3)return 5;
 try{a.push(7,...rest,fail());}catch{}
 if(a.length!==12)return 6;
 const words:string[]=['é'];words.push(...'🙂,x'.split(','));
 return words.length===3 && words[1]==='🙂' ? 0:7;
}
"#;
    let mut program = Program::checked(source, "array-search.ts");
    assert_eq!(program.outcome("search", &[]), (0.0, 0));
    assert_eq!(program.outcome("push", &[]), (0.0, 0));
}

#[test]
fn number_coercion_uses_javascript_notation() {
    let source = r#"
export function run():number {
 const values:number[]=[-0,NaN,Infinity,-Infinity,1e-7,1e-6,1e20,1e21];
 const expected:string[]=['0','NaN','Infinity','-Infinity','1e-7','0.000001','100000000000000000000','1e+21'];
 for(let i=0;i<values.length;i++) {
   const value=values[i];
   if(String(value)!==expected[i] || ''+value!==expected[i] || `${value}`!==expected[i])return i+1;
 }
 return 0;
}
"#;
    assert_eq!(Program::new(source).call("run", &[]).0[0].unwrap_f64(), 0.0);
}

#[test]
fn checked_json_tuples_are_arrays_and_preserve_numeric_element_order() {
    let source = r#"
interface Rule {filter:Array<[string,string]>}
export function run():number {
    const rules:Rule[] = JSON.parse('[{"filter":[["MenuRowName","Lunch"],["showInMenu","true"]]}]');
    if (rules[0].filter[0][1] !== 'Lunch') return 1;
    if (JSON.stringify(rules) !== '[{"filter":[["MenuRowName","Lunch"],["showInMenu","true"]]}]') return 2;
    const wide:[number,number,number,number,number,number,number,number,number,number,number] = JSON.parse('[0,1,2,3,4,5,6,7,8,9,10]');
    if (JSON.stringify(wide) !== '[0,1,2,3,4,5,6,7,8,9,10]') return 3;
    const record:{_0:number,_1:number}={_0:1,_1:2};
    if (JSON.stringify(record) !== '{"_0":1,"_1":2}') return 4;
    return 0;
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (0.0, 0));
}

#[test]
fn closed_aggregate_widening_preserves_aliases_and_null() {
    let source = r#"
interface Row {name:string}
interface Native {rows:Array<{row:Row|null}>}
interface Boundary {rows:Array<{row:Row|null|undefined}>}
function widen(value:Native):Boundary {return value;}
export function run():number {
    const native:Native = {rows:[{row:null},{row:{name:'before'}}]};
    const boundary = widen(native);
    if (boundary.rows[0].row !== null || boundary.rows[0].row === undefined) return 1;
    boundary.rows[1].row.name = 'changed';
    if (native.rows[1].row.name !== 'changed') return 2;
    native.rows[0].row = {name:'new'};
    if (boundary.rows[0].row.name !== 'new') return 3;
    boundary.rows[2] = {row:null};
    if (native.rows.length !== 3 || native.rows[2].row !== null) return 4;
    for (let i=0; i<10000; i++) { const scratch:Native = {rows:[{row:{name:'temporary'}}]}; }
    return boundary.rows[0].row.name.length;
}
"#;
    assert_eq!(
        Program::checked(source, "widen.ts").outcome("run", &[]),
        (3.0, 0)
    );
}

#[test]
fn existing_source_types_take_precedence_over_sdk_name_matches() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("domain.ts"),
        "export interface Options { label: string }",
    )?;
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "local.wit",
        "package test:local; world run {record options {count:u32} export run:func(value:options)->u32;}",
    )?;
    let world = resolve.select_world(&[package], Some("run"))?;
    perry_wit::compile_for_world(
        "import type { Options } from './domain.ts'; import type { Options as WitOptions } from './types/world'; function size(value:Options):number{return value.label.length;} export function run(value:WitOptions):number{return size({label:'abc'}) + value.count;}",
        &directory.path().join("main.ts").to_string_lossy(),
        &resolve,
        world,
        true,
    )?;
    Ok(())
}

#[test]
fn early_returns_narrow_variants_and_optional_records_are_truthy() {
    let source = r#"
type Choice = {tag:'text';val:string} | {tag:'number';val:number};
function size(value:Choice):number {
    try {
        if (value.tag === 'text') return value.val.length;
        return value.val + 1;
    } finally { const ignored = 0; }
}
function present(value:{n:number} | null | undefined):number { return value ? value.n : 10; }
interface Options { query?: { search?: string } }
function label(options:Options):string {return options.query?.search?.trim().toLowerCase() ?? '';}
function checked(options:Options, visits:number[]):Options {visits.push(1);return options;}
function nested():boolean {
 const visits:number[]=[];
 const options:Options={query:{search:' OK '}};
 const value=checked(options,visits).query?.search?.trim().toLowerCase() ?? '';
 return label({})==='' && label({query:{}})==='' && value==='ok' && visits.length===1;
}
export function run():number {if(!nested())return -1;return size({tag:'text',val:'abc'}) + size({tag:'number',val:4}) + present({n:7}) + present(null) + present(undefined);}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (35.0, 0));
}

#[test]
fn json_scalar_equality_does_not_coerce_types() {
    let source = r#"
function matches(text: string, expected: string | null | undefined): boolean {
    return JSON.parse(text).value === expected;
}
export function run(): number {
    if (!matches('{"value":"yes"}', 'yes')) return 1;
    if (matches('{"value":1}', '1')) return 2;
    if (!matches('{"value":null}', null)) return 3;
    if (!matches('{}', undefined)) return 4;
    if (matches('{}', null) || matches('{"value":null}', undefined)) return 5;
    const values = JSON.parse('[1,"1",true,false,0,{},[]]');
    if (values[0] !== 1 || values[1] === 1 || values[2] !== true) return 6;
    if (values[3] === 0 || values[4] === false || values[5] === true) return 7;
    if (values[6] === 0 || '1' !== values[1] || values[4] === NaN) return 8;
    return 0;
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (0.0, 0));
}

#[test]
fn header_splitting_and_scalar_truthiness_preserve_values() {
    let source = r#"
function present(value: string | undefined): number { return !value ? 0 : value.length; }
function splitHeader(header: { value: string }): string[] { return header.value.split(','); }
export function run(n: number): number {
    const parts = splitHeader({ value: ',max-age=1,private,' });
    const characters = 'é🙂'.split('');
    const missing = parts[8];
    const blank = ''.split('');
    const whole = ''.split(',');
    let result = present(parts[0]) + present(parts[1]) + present(missing);
    if (!n) result += 100;
    const parsed = JSON.parse('[false,true,0,1,"","x",null,[],{}]');
    for (let i = 0; i < 9; i++) { if (parsed[i]) result += 1; }
    return result + parts.length * 1000 + characters.length * 10000 + blank.length + whole.length;
}
"#;
    let mut program = Program::new(source);
    for input in [0.0, -0.0, f64::NAN] {
        assert_eq!(program.throwing("run", input), (24115.0, 0));
    }
    for input in [1.0, -1.0, f64::INFINITY] {
        assert_eq!(program.throwing("run", input), (24015.0, 0));
    }
}

#[test]
fn wit_interface_type_aliases_resolve_in_their_own_modules() -> Result<()> {
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "scoped.wit",
        "package test:scoped; interface left {record options {count:u32}} interface right {record options {label:string}} world run {use left.{options as left-options}; use right.{options as right-options}; export run:func(left:left-options,right:right-options)->u32;}",
    )?;
    let world = resolve.select_world(&[package], Some("run"))?;
    perry_wit::compile_for_world(
        "import type { Options as Left } from 'test:scoped/left'; import type { Options as Right } from 'test:scoped/right'; export function run(left:Left,right:Right):number {return left.count + right.label.length;}",
        "scoped.ts",
        &resolve,
        world,
        true,
    )?;
    Ok(())
}

#[test]
fn quoted_closed_record_fields_survive_checked_json_conversion() {
    let source = r#"
interface Days { "0"?: number[]; "a-b": number; "": string; "漢": number }
export function run(): number {
    const days: Days = JSON.parse('{"0":[7],"a-b":11,"":"yes","漢":13}');
    const encoded = JSON.stringify(days);
    const copy: Days = JSON.parse(encoded);
    const first = copy["0"];
    if (first === undefined) return -1;
    return first[0] + copy["a-b"] + copy[""].length + copy["漢"];
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (34.0, 0));
}

#[test]
fn json_pointer_lookup_and_optional_tuple_literals() {
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

#[test]
fn generic_interfaces_aliases_and_defaults_resolve_to_concrete_shapes() {
    let source = r#"
interface Item { n:number }
interface Pair<Item,Other=Item> { a:Item; b:Other }
interface Page<T> {items:T[];next:string|null}
interface Base<T> {value:T}
interface Entry<T=string> extends Base<T> {}
type Box<T> = {item:T};
interface Link<T> {value:T;next:Link<T>|null}
function size(page:Page<Entry>) {const items = page.items; const first = items[0]; return first.value.length;}
function unbox(box:Box<number>):number {return box.item;}
export function run():number {
 const value:Pair<string,Item> = {a:'x',b:{n:17}};
 const defaults:Pair<string> = {a:'a',b:'bc'};
 const page:Page<Entry> = {items:[{value:'hello'}],next:null};
 const link:Link<number> = {value:3,next:null};
 return value.b.n + defaults.b.length + size(page) + unbox({item:2}) + link.value;
}
"#;
    let values = Program::new(source).call("run", &[]).0;
    assert_eq!(values.len(), 1, "the known projection cannot throw");
    assert_eq!(values[0].unwrap_f64(), 29.0);
}

#[test]
fn unsupported_iterators_have_direct_diagnostics() {
    for expression in ["new Headers()", "new URLSearchParams()"] {
        let source = format!(
            "export function run():number {{for(const value of {expression}){{}} return 0;}}"
        );
        let error = perry_wit::compile_core(&source, "iteration.ts").unwrap_err();
        assert!(
            format!("{error:#}").contains("synchronous for...of requires"),
            "{error:#}"
        );
    }
}

#[test]
fn text_pipeline_join_replace_regex_split_and_non_throwing_char_loops() {
    let source = r#"
export function charLoop(s: string): number {
    let count = 0;
    for (let i = 0; i < s.length; i++) {
        const ch = s[i];
        if (ch === '-') count++;
    }
    return count;
}

export function run(): number {
    const words = '  hello,  world;foo\tbar  '.trim().split(/[,;\s]+/);
    if (words.join('-') !== 'hello-world-foo-bar') return 1;
    if (words.join('') !== 'helloworldfoobar') return 2;
    if (words.join() !== 'hello,world,foo,bar') return 3;
    const nums: number[] = [1, 2, 3];
    if (nums.join(':') !== '1:2:3') return 4;
    const empty: string[] = [];
    if (empty.join(',') !== '') return 5;
    const opts: Array<string | null | undefined> = ['a', null, undefined, 'b'];
    if (opts.join('-') !== 'a---b') return 6;

    if ('a-b-c'.replace('-', '_') !== 'a_b-c') return 7;
    if ('a-b-c'.replaceAll('-', '_') !== 'a_b_c') return 8;
    if ('hello   world\tfoo'.replace(/\s+/, ' ') !== 'hello world\tfoo') return 9;
    if ('hello   world\tfoo'.replace(/\s+/g, ' ') !== 'hello world foo') return 10;
    if ('hello   world\tfoo'.replaceAll(/\s+/g, '_') !== 'hello_world_foo') return 11;
    if ('abc'.replace(/b/, '[$&:$`:$\':$$]') !== 'a[b:a:c:$]c') return 12;
    if ('ab'.replaceAll('', '-') !== '-a-b-') return 13;

    if ('  hi  '.trimStart() !== 'hi  ' || '  hi  '.trimEnd() !== '  hi') return 14;
    if ('5'.padStart(3, '0') !== '005' || '5'.padEnd(3, '0') !== '500') return 15;
    if ('abc'.padStart(6, 'xy') !== 'xyxabc' || 'abc'.padEnd(6, 'xy') !== 'abcxyx') return 16;
    if ('é🙂z'.at(0) !== 'é' || 'é🙂z'.at(-2) !== '🙂' || 'é🙂z'.at(9) !== undefined) return 17;

    let clean = '';
    for (const ch of 'a-b c') {
        clean += ch === '-' || ch === ' ' ? '_' : ch;
    }
    if (clean !== 'a_b_c') return 18;
    return charLoop('a-b-c');
}
"#;
    let mut program = Program::checked(source, "text-pipeline.ts");
    let char_loop = program.call(
        "charLoop",
        &[wasmtime::Val::I32(0)],
    );
    assert_eq!(
        char_loop.0.len(),
        1,
        "in-bounds character loop must not emit an exception status return"
    );
    assert_eq!(program.outcome("run", &[]), (2.0, 0));

    let err = perry_wit::compile_core(
        "export function run(): string { return 'a'.replaceAll(/a/, 'b'); }",
        "replace-all-non-global.ts",
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("global"), "{err:#}");
}

#[test]
fn typed_record_spread_and_destructuring_across_bindings_loops_and_params() {
    let source = r#"
interface Item { id: number; name: string; plu: string; price?: number }
interface Pair { a: number; b: number; c?: number }

function sumPair({ a, b: second, c = 10 }: Pair): number {
    return a + second + c;
}

export function run(): number {
    const base: Item = { id: 1, name: 'Soup', plu: '100', price: 500 };
    const updated: Item = { ...base, plu: '200' };
    if (updated.id !== 1 || updated.name !== 'Soup' || updated.plu !== '200' || updated.price !== 500) return 1;
    if (base.plu !== '100') return 2;

    const { id, name: label, price = 0, ...rest } = updated;
    if (id !== 1 || label !== 'Soup' || price !== 500 || rest.plu !== '200') return 3;

    const items: Pair[] = [{ a: 1, b: 2 }, { a: 3, b: 4, c: 5 }];
    let total = 0;
    for (const { a, b: beta, c = 10 } of items) {
        total += a + beta + c;
    }
    if (total !== 25) return 4;

    const mapped = items.map(({ a, b, c = 1 }) => a * 10 + b + c);
    if (mapped[0] !== 13 || mapped[1] !== 39) return 5;

    if (sumPair({ a: 2, b: 3 }) !== 15 || sumPair({ a: 2, b: 3, c: 4 }) !== 9) return 6;
    return 0;
}
"#;
    let mut program = Program::checked(source, "record-spread-destructure.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn default_and_rest_parameters_on_functions_and_closures() {
    let source = r#"
function formatTag(label = 'item', count = 1, suffix: string | null = null): string {
    return `${label}:${count}:${suffix ?? 'none'}`;
}

function sumWithBase(base = 10, ...values: number[]): number {
    return values.reduce((acc, v) => acc + v, base);
}

export function run(): number {
    if (formatTag() !== 'item:1:none') return 1;
    if (formatTag(undefined, 5) !== 'item:5:none') return 2;
    if (formatTag('box', undefined, 'kg') !== 'box:1:kg') return 3;
    if (formatTag('box', 2, null) !== 'box:2:none') return 4;

    if (sumWithBase() !== 10) return 5;
    if (sumWithBase(5, 1, 2, 3) !== 11) return 6;

    const arr: Array<number | undefined> = [1, undefined, 3];
    const filled = arr.map((x = 9) => x * 2);
    if (filled[0] !== 2 || filled[1] !== 18 || filled[2] !== 6) return 7;
    return 0;
}
"#;
    let mut program = Program::checked(source, "default-rest-params.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn temporal_plain_date_subtract_and_compact_helper_footprint() {
    let source = r#"
export function run(): number {
    const leap = Temporal.PlainDate.from('2024-02-29');
    const prev = leap.subtract({ days: 1 });
    const next = leap.add({ days: 1 });
    if (prev.toString() !== '2024-02-28') return 1;
    if (next.toString() !== '2024-03-01') return 2;
    if (leap.year !== 2024 || leap.month !== 2 || leap.day !== 29 || leap.dayOfWeek !== 4) return 3;

    const dt = Temporal.PlainDateTime.from('2024-03-01T12:30:00');
    const dtPrev = dt.subtract({ days: 2 });
    if (dtPrev.toString() !== '2024-02-28T12:30:00') return 4;

    const inst = Temporal.Instant.from('1970-01-01T00:00:02.500Z');
    if (inst.epochMilliseconds !== 2500 || inst.toString() !== '1970-01-01T00:00:02.5Z') return 5;
    if (Temporal.Now.timeZoneId() !== 'UTC') return 6;
    return 0;
}
"#;
    let mut program = Program::checked(source, "temporal-compact.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));

    let date_only = perry_wit::compile_core(
        "export function check(): number { const d = Temporal.PlainDate.from('2024-02-29').subtract({ days: 1 }); return d.day; }",
        "plain-date-only.ts",
    )
    .unwrap();
    assert!(
        date_only.len() < 24_576,
        "PlainDate-only module should remain compact, got {} bytes",
        date_only.len()
    );
}

#[test]
fn smaller_gaps_inference_logical_values_utility_types_and_collection_methods() {
    let source = r#"
type A = { tag: 'a'; val: number };
type B = { tag: 'b'; val: string };
type C = { tag: 'c'; val: boolean };
type AB = A | B;
type ABC = AB | C;
type OnlyAB = Exclude<ABC, C>;
type JustA = Extract<ABC, { tag: 'a' }>;
type DefiniteString = NonNullable<string | null | undefined>;

function checkVariant(x: ABC): number {
    if (x.tag === 'a' && x.val > 5) return x.val;
    if (x.tag !== 'b' || x.val.length === 0) return 0;
    return x.val.length;
}

function reduceAndForEach(items: number[]): number {
    let sum = 0;
    items.forEach((x, idx) => { sum += x + idx; });
    const prod = items.reduce((acc, x) => acc * x, 1);
    const bare = items.reduce((acc, x) => acc + x);
    return sum + prod + bare;
}

export function run(): number {
    const map = new Map<string, { label: string }>();
    map.set('k', { label: 'found' });
    const hit = map.get('k')?.label ?? null;
    const miss = map.get('missing')?.label ?? null;
    if (hit !== 'found' || miss !== null) return 1;

    const s1: DefiniteString = '' || 'fallback';
    const s2: DefiniteString = 'primary' || 'fallback';
    const s3: DefiniteString = 'left' && 'right';
    const s4: DefiniteString = '' && 'right';
    if (s1 !== 'fallback' || s2 !== 'primary' || s3 !== 'right' || s4 !== '') return 2;

    const ab: OnlyAB = { tag: 'a', val: 9 };
    const ja: JustA = { tag: 'a', val: 3 };
    if (checkVariant(ab) !== 9 || ja.val !== 3 || checkVariant({ tag: 'b', val: 'hi' }) !== 2) return 3;

    const nums: number[] = [2, 3, 4];
    if (reduceAndForEach(nums) !== (2 + 0 + 3 + 1 + 4 + 2) + 24 + 9) return 4;
    if (nums.at(-1) !== 4 || nums.at(0) !== 2 || nums.at(5) !== undefined) return 5;
    if (nums.findLast(x => x < 4) !== 3 || nums.findLastIndex(x => x < 4) !== 1) return 6;
    if (nums.findLast(x => x > 9) !== undefined || nums.findLastIndex(x => x > 9) !== -1) return 7;

    const rev = nums.toReversed();
    if (rev.join(',') !== '4,3,2' || nums.join(',') !== '2,3,4') return 8;
    const copy = nums.slice(1, -1);
    if (copy.join(',') !== '3') return 9;
    const mutRev = [1, 2, 3].reverse();
    if (mutRev.join(',') !== '3,2,1') return 10;

    const nested: number[][] = [[1, 2], [], [3]];
    if (nested.flat().join(',') !== '1,2,3') return 11;

    const hex = (255).toString(16);
    const bin = (-10).toString(2);
    if (hex !== 'ff' || bin !== '-1010') return 12;

    return 0;
}
"#;
    let mut program = Program::checked(source, "smaller-gaps.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

