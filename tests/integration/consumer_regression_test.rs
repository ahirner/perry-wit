//! Regressions from consumer programs: checked JSON shapes, narrowing, truthiness and type
//! resolution across modules.
use anyhow::Result;

use crate::core_program::Program;

#[test]
fn dense_array_iteration_callbacks_and_search() {
    let source = r#"
function twice(n:number):number {return n*2;}
export function run():number {
 const words:string[] = [];
 words.push('a'); words.push('b');
 let text = '';
 for (const word of words) {text += word;}
 for (const word of 'c,d'.split(',')) {text += word;}
 if(text !== 'abcd' || !words.includes('b') || words.indexOf('b',-1)!==1) return 1;
 let calls = 0;
 const numbers:number[] = [1,2,3];
 const mapped = numbers.map((n,i,a) => {calls++; if(i === 0) a.push(4); return n+i;});
 const filtered = mapped.filter(n => n>1).map(twice);
 if(calls !== 3 || mapped.length !== 3 || filtered[0] !== 6 || filtered[1] !== 10) return 2;
 const special:number[] = [NaN,0];
 if(!special.includes(NaN) || special.indexOf(NaN)!==-1 || !special.includes(-0)) return 3;
 return 0;
}

"#;
    assert_eq!(
        Program::checked(source, "array-consumer.ts")
            .call("run", &[])
            .0[0]
            .unwrap_f64(),
        0.0
    );
}

#[test]
fn array_callbacks_preserve_effects_identity_exceptions_and_loop_exits() {
    let source = r#"
interface Item {n:number}
export function run():number {
 const item:Item={n:1};
 const other:Item={n:1};
 const objects:Item[]=[item];
 if(!objects.includes(item) || objects.includes(other) || objects.indexOf(other)!==-1) return 1;
 const values:number[]=[1,2];
 let visits=0;
 for(const n of values) {
   visits++;
   if(n===1) {values.push(3);continue;}
   if(n===3) break;
 }
 if(visits!==3) return 2;
 let trace=0;
 try {values.map((n,i,a)=>{trace=trace*10+n;if(i===1)throw new Error('stop');a[1]=4;return n;});return 3;}
 catch(e){if(e.message!=='stop')return 4;}
 if(trace!==14) return 5;
 const mapped=values.map(n=>{try {return n*2;}finally {trace++;}});
 if(mapped[1]!==8 || trace!==17) return 6;
 if(!values.includes(1,NaN) || values.includes(1,Infinity) || !values.includes(1,-Infinity)) return 7;
 if(values.indexOf(4,-2.8)!==1 || values.indexOf(1,0.8)!==0 || values.includes(1,-1)) return 8;
 return 0;
}
"#;
    let mut program = Program::checked(source, "array-effects.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
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
export function run():number {return size({tag:'text',val:'abc'}) + size({tag:'number',val:4}) + present({n:7}) + present(null) + present(undefined);}
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
    if (a['00'] !== undefined) return -3;
    const text = 'é🙂';
    if (text[-1] !== undefined || text[2] !== undefined || text[0.5] !== undefined) return -4;
    if (text[1] !== '🙂') return -5;
    return raw[0] + first[''] + a['length'] + length({extra: [['status', {jsonText: 'ok'}]]});
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (21.0, 0));
}

#[test]
fn array_search_arguments_and_optional_callbacks_match_javascript() {
    let source = r#"
const values:number[]=[1];
function start():number {values.push(2);return 0;}
function nothing(n:number):void {}
function failArgument():string {throw new Error('argument');}
export function run():number {
 if(!values.includes(2,start()))return 1;
 const optional:(number|undefined)[]=[undefined,NaN,1];
 if(!optional.includes(NaN) || !optional.includes(undefined) || optional.indexOf(NaN)!==-1)return 2;
 const mixed:(string|number)[]=['x',NaN];
 if(!mixed.includes(NaN) || mixed.indexOf(NaN)!==-1)return 3;
 const mapped=values.map(n=>{if(n===1)return 'yes';});
 if(mapped[0]!=='yes' || mapped[1]!==undefined)return 4;
 if(values.filter(nothing).length!==0)return 5;
 const bare=values.map(n=>{if(n===1)return;return 'value';});
 if(bare[0]!==undefined || bare[1]!=='value')return 6;
 const pushed:number[]=[];
 if(pushed.push(1,pushed.length)!==2 || pushed[1]!==0 || pushed.push()!==2)return 7;
 const holder:{items:string[]}={items:[]};
 try {holder.items.push('first',failArgument());}catch(e){}
 if(holder.items.length!==0)return 8;
 return 0;
}
"#;
    assert_eq!(
        Program::checked(source, "array-optionals.ts").outcome("run", &[]),
        (0.0, 0)
    );
}

#[test]
fn generic_interfaces_aliases_and_defaults_resolve_to_concrete_shapes() {
    let source = r#"
interface Item { n:number }
interface Pair<Item,Other=Item> { a:Item; b:Other }
interface Page<T> {items:T[];next:string|null}
interface Entry<T=string> {value:T}
type Box<T> = {item:T};
interface Link<T> {value:T;next:Link<T>|null}
function size(page:Page<Entry>):number {return page.items[0].value.length;}
function unbox(box:Box<number>):number {return box.item;}
export function run():number {
 const value:Pair<string,Item> = {a:'x',b:{n:17}};
 const defaults:Pair<string> = {a:'a',b:'bc'};
 const page:Page<Entry> = {items:[{value:'hello'}],next:null};
 const link:Link<number> = {value:3,next:null};
 return value.b.n + defaults.b.length + size(page) + unbox({item:2}) + link.value;
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (29.0, 0));
}

#[test]
fn callbacks_return_through_switch_and_unconditional_loops() {
    let source = r#"
export function run():number {
 const values:number[]=[1,2,3];
 const a:number[]=values.map(n=>{switch(n){case 1:return 7;case 2:default:return n;}});
 const b:number[]=values.map(n=>{while(true){return n*2;}});
 let text='';
 for(const scalar of 'é🙂a'){text+=scalar+':';}
 if(text!=='é:🙂:a:')return 1;
 return a[0]+a[1]+a[2]+b[2];
}
"#;
    assert_eq!(
        Program::new(source).call("run", &[]).0[0].unwrap_f64(),
        18.0
    );
}

#[test]
fn push_spreads_snapshot_arguments_before_mutation() {
    let source = r#"
const a:number[]=[1,2];
const rest:number[]=[3,4];
function later():number {rest[0]=8;return a.length;}
function fail():number {throw new Error('argument');}
export function run():number {
 if(a.push(0,...rest,later())!==6)return 1;
 if(a[3]!==3 || a[4]!==4 || a[5]!==2)return 2;
 if(a.push(...a)!==12 || a[9]!==3)return 3;
 try{a.push(7,...rest,fail());}catch{}
 if(a.length!==12)return 4;
 const words:string[]=['é'];words.push(...'🙂,x'.split(','));
 return words.length===3 && words[1]==='🙂' ? 0:5;
}
"#;
    assert_eq!(
        Program::checked(source, "push-spread.ts").outcome("run", &[]),
        (0.0, 0)
    );
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
