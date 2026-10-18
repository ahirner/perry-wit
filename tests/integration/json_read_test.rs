//! Graph-based JSON schema conversion, serialization, ownership and resource bounds.
use crate::core_program::{Counter, Program};

/// Parsing then serializing each document gives Node's text: canonical escapes, array-index
/// keys first, a repeated key in its first position with its last value, ECMAScript numbers
/// and nesting up to the depth limit. Compiled object literals quote their keys the same way.
#[test]
fn graph_round_trips_match_node() {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut compiled = String::new();
    // Keys of every escape class, and one longer than a string chunk.
    let keys = (0..32).map(char::from).map(String::from).chain([
        "\"\\".into(),
        "é🙂".into(),
        "x".repeat(260),
    ]);
    for key in keys {
        let object = format!("{{{}:7}}", serde_json::to_string(&key).unwrap());
        let text = serde_json::to_string(&object).unwrap();
        compiled.push_str(&format!(
            "if(JSON.stringify({object})!=={text})return -2;\n"
        ));
        pairs.push((object.clone(), object));
    }
    // Small and large objects around the scratch-table limits, with escaped duplicates.
    for count in [0, 1, 16, 28, 29, 32, 33, 54, 80, 128, 129] {
        let mut fields: Vec<_> = (0..count).map(|i| format!("\"key{i}\":{i}")).collect();
        if count > 0 {
            fields.push("\"\\u006bey0\":9".into());
        }
        fields.extend(["\"2\":2", "\"1\":1", "\"n\":7", "\"\\u006e\":8"].map(String::from));
        let mut expected = vec!["\"1\":1".to_string(), "\"2\":2".into()];
        expected.extend((0..count).map(|i| format!("\"key{i}\":{}", if i == 0 { 9 } else { i })));
        expected.push("\"n\":8".into());
        pairs.push((
            format!("{{{}}}", fields.join(",")),
            format!("{{{}}}", expected.join(",")),
        ));
    }
    // Decoded escapes are quoted again canonically.
    let decoded = "a\u{0}\u{1}\u{8}\u{c}\n\r\t\"\\/é🙂\u{2028}z";
    let escaped = r#"a\u0000\u0001\b\f\n\r\t\"\\\/\u00e9\ud83d\ude42\u2028z"#;
    let members = |value: &str| {
        (0..80)
            .map(|i| format!("\"key{i}\":{value}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    pairs.push((
        format!("{{\"n\":1,{}}}", members(&format!("\"{escaped}\""))),
        format!(
            "{{\"n\":1,{}}}",
            members(&serde_json::to_string(decoded).unwrap())
        ),
    ));
    pairs.push((
        r#"{"n":1,"raw":[0,-0,0.0,-0.0,1e999,1e-999,-1e999,999999999999999,-999999999999999,9999999999999999,1e20,1e21,1e-6,1e-7,1.00,-0.1]}"#.into(),
        r#"{"n":1,"raw":[0,0,0,0,null,0,null,999999999999999,-999999999999999,10000000000000000,100000000000000000000,1e+21,0.000001,1e-7,1,-0.1]}"#.into(),
    ));
    // Mixed arrays and objects 126 deep; one more level inside `[[…]]` exceeds the limit.
    let mut nested = "0".to_string();
    for depth in 0..126 {
        nested = if depth % 2 == 0 {
            format!("[false,{nested},null]")
        } else {
            format!(r#"{{"1":1,"next":{nested},"tail":"é🙂\"}}],\\"}}"#)
        };
    }
    let deep = pairs.len();
    pairs.push((
        format!(r#"{{"n":1,"before":[],"deep":{nested},"after":{{"1":1,"2":2}}}}"#),
        String::new(),
    ));
    pairs[deep].1 = pairs[deep].0.clone();
    pairs.push((nested.clone(), nested));
    pairs.push((
        r#"{"n":3,"tail":[{"2":2,"1":1},false]}"#.into(),
        r#"{"n":3,"tail":[{"1":1,"2":2},false]}"#.into(),
    ));
    let (inputs, outputs): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let source = format!(
        r#"
const inputs:string[]={};
const outputs:string[]={};
export function run():number {{
{compiled}
 for(let i=0;i<inputs.length;i++) if(JSON.stringify(JSON.parse(inputs[i]))!==outputs[i]) return i+1;
 try {{JSON.stringify([[JSON.parse(inputs[{deep}])]]);return -1;}}catch(e){{}}
 return 0;
}}
"#,
        serde_json::to_string(&inputs).unwrap(),
        serde_json::to_string(&outputs).unwrap(),
    );
    let mut program = Program::checked(&source, "round-trips.ts");
    program.stress_collection(true);
    // Only the module's document tables stay live between runs.
    let mut live = None;
    for _ in 0..3 {
        let (failed, status) = program.outcome("run", &[]);
        assert_eq!(status, 0);
        assert_eq!(failed, 0.0, "document {failed}");
        let bytes = program.counter(Counter::LiveBytes);
        assert_eq!(*live.get_or_insert(bytes), bytes);
    }
}

/// Typed decoding validates only the final value of a repeated member, honours the depth
/// limit in skipped values, decodes numeric extremes and finds escaped, Unicode and long keys
/// among many fields.
#[test]
fn typed_json_keeps_final_members_and_rejects_invalid_documents() {
    const DUPLICATES: &str = r#"
type Result<T> = {tag:'ok';val:T}|{tag:'err';val:string};
interface Item {n:number}
interface Envelope {result:Result<Item>}
interface Row {name:string; inner:Item; extra?:string}
export function encoding():number {
 const text = new TextDecoder().decode(new Uint8Array([34,255,34]));
 const typed:string = JSON.parse(text);
 if(typed !== '�') return 1;
 if(JSON.stringify(JSON.parse(text)) !== '\"�\"') return 2;
 try {const invalid = new TextDecoder('utf-8',{fatal:true}).decode(new Uint8Array([255]));return 3;}
 catch(error) {if(!(error instanceof TypeError))return 4;}
 return 0;
}
export function duplicates():number {
 const tagged:Envelope=JSON.parse('{"result":{"val":{"n":4},"tag":"ok"}}');
 if(tagged.result.tag !== 'ok') return 1;
 if(tagged.result.val.n !== 4) return 1;
 const duplicate:Envelope=JSON.parse('{"result":{"val":false,"tag":false,"tag":"ok","val":{"n":5}}}');
 if(duplicate.result.tag !== 'ok') return 2;
 if(duplicate.result.val.n !== 5) return 2;
 if(JSON.stringify(duplicate)!=='{"result":{"tag":"ok","val":{"n":5}}}')return 3;
 const overwritten:Envelope=JSON.parse('{"result":{"tag":"missing"},"result":{"tag":"err","val":"yes"}}');
 if(overwritten.result.tag !== 'err') return 4;
 if(overwritten.result.val !== 'yes') return 4;
 if(JSON.stringify(overwritten)!=='{"result":{"tag":"err","val":"yes"}}')return 5;
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"missing"},"ignored":[1,]}');return 6;}
 catch(e) {if(!(e instanceof SyntaxError) || !e.message.includes('Invalid JSON at byte')) return 7;}
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"ok"}}');return 8;} catch(e) {}
 const row:Row = JSON.parse('{"name":false,"inner":{"n":false},"name":"é🙂","inner":{"n":3}}');
 if(row.name.length !== 2 || row.inner.n !== 3 || row.extra !== undefined) return 9;
 if(JSON.stringify(row)!=='{"name":"é🙂","inner":{"n":3}}')return 10;
 try {const bad:Row = JSON.parse('{"name":"ok","inner":{"n":1},"name":false}'); return 11;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":false,"inner":{},"ignored":[1,]}'); return 12;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok","inner":null}'); return 13;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok"}'); return 14;} catch(e) {}
 const fixed:Row = JSON.parse('{"name":"ok","inner":false,"inner":{"n":4}}');
 return fixed.inner.n - 4;
}
"#;
    let nested = |depth: usize| format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    let document = |n: &str, text: &str, depth| {
        let text = format!(r#"{{"n":{n},"text":"{text}","ignored":{}}}"#, nested(depth));
        serde_json::to_string(&text).unwrap()
    };
    let (good, bad) = (document("-0", "é🙂", 126), document("0", "", 127));
    let limits = format!(
        r#"
interface Limits {{n:number;text:string}}
export function limits():number {{
 const row:Limits=JSON.parse({good});
 if(1/row.n !== -Infinity || row.text.length !== 2) return 1;
 try {{const bad:Limits=JSON.parse({bad});return 2;}}catch(e){{}}
 const over:Limits=JSON.parse('{{"n":1e400,"text":"\\u00e9\\ud83d\\ude42"}}');
 if(over.n !== Infinity || over.text !== row.text) return 3;
 try {{const bad:Limits=JSON.parse('{{"n":0,"text":"\\ud800"}}');return 4;}}catch(e){{}}
 try {{const bad:Limits=JSON.parse('{{"n":0,"text":""}} true');return 5;}}catch(e){{}}
 const escaped:Limits=JSON.parse('{{"\\u006e":7,"text":"ok"}}');
 return escaped.n-7;
}}
"#
    );
    // Forty-three typed fields, given in reverse after an invalid first `field0`.
    let mut keys: Vec<_> = (0..40).map(|index| format!("field{index}")).collect();
    keys.extend(["é🙂".into(), "".into(), "x".repeat(260)]);
    let quoted: Vec<_> = keys
        .iter()
        .map(|key| serde_json::to_string(key).unwrap())
        .collect();
    let declared: Vec<_> = quoted.iter().map(|key| format!("{key}:number")).collect();
    let mut members: Vec<_> = quoted
        .iter()
        .enumerate()
        .rev()
        .map(|(index, key)| format!("{key}:{index}"))
        .collect();
    members.insert(0, "\"field0\":false".into());
    members.push("\"ignored\":{\"field0\":false}".into());
    members.push("\"\\u0066ield1\":17".into());
    let input = serde_json::to_string(&format!("{{{}}}", members.join(","))).unwrap();
    let checks: String = quoted
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let expected = if index == 1 { 17 } else { index };
            format!("if(value[{key}]!=={expected})return 1;")
        })
        .collect();
    let fields = format!(
        "export function fields():number {{const value:{{{}}}=JSON.parse({input});{checks}return 0;}}",
        declared.join(";")
    );
    let source = format!("{DUPLICATES}{limits}{fields}");
    let mut program = Program::checked(&source, "typed-validation.ts");
    program.stress_collection(true);
    for export in ["duplicates", "limits", "fields", "encoding"] {
        assert_eq!(program.outcome(export, &[]), (0.0, 0), "{export}");
    }
}

/// Typed values overlay current storage on retained members in their insertion order.
#[test]
fn typed_stringify_keeps_undeclared_members_and_mutations() {
    let source = r#"
interface Inner {n:number}
interface Row {name:string; inner:Inner; values:number[]; added?:number; another?:number}
type Result<T>={tag:'ok';val:T}|{tag:'err';val:string};
interface Part {id:number; values:number[]}
interface Document {one:Part; two:Part}
let saved:Document|undefined;
export function members():number {
 const row:Row = JSON.parse('{ "extra": {"z":1,"2":2,"1":1,"z":3}, "name":"old", "inner":{"n":2,"other":"\\u0061"}, "values":[1,2], "name":"new", "numeric":-0, "overflow":1e400 }');
 if (JSON.stringify(row) !== '{"extra":{"1":1,"2":2,"z":3},"name":"new","inner":{"n":2,"other":"a"},"values":[1,2],"numeric":0,"overflow":null}') return 1;
 const alias = row.inner;
 alias.n = 4;
 row.values.push(3);
 row.values[0] = 5;
 row.name = 'changed';
 row.another = 8;
 row.added = 7;
 if (JSON.stringify(row) !== '{"extra":{"1":1,"2":2,"z":3},"name":"changed","inner":{"n":4,"other":"a"},"values":[5,2,3],"numeric":0,"overflow":null,"another":8,"added":7}') return 2;
 row.another = undefined;
 if (JSON.stringify(row) !== '{"extra":{"1":1,"2":2,"z":3},"name":"changed","inner":{"n":4,"other":"a"},"values":[5,2,3],"numeric":0,"overflow":null,"added":7}') return 3;
 row.inner = {n:9};
 if (JSON.stringify(row.inner) !== '{"n":9}' || JSON.stringify(alias) !== '{"n":4,"other":"a"}') return 4;
 const optional:{n?:number}=JSON.parse('{"n":1,"unknown":false,"unknown":{"escaped":"\\u00e9"}}');
 optional.n=undefined;
 if(JSON.stringify(optional)!=='{"unknown":{"escaped":"é"}}')return 5;
 optional.n=3;
 if(JSON.stringify(optional)!=='{"n":3,"unknown":{"escaped":"é"}}')return 6;
 if(JSON.stringify([-0,Infinity,-Infinity,NaN,1e-7,1e21])!=='[0,null,null,null,1e-7,1e+21]')return 7;
 return 0;
}
export function tuples():number {
 const tuple:[number,string|undefined]=JSON.parse('[1]');
 if(JSON.stringify(tuple)!=='[1,null]')return 1;
 tuple[1]='added';
 if(JSON.stringify(tuple)!=='[1,"added"]')return 2;
 const extra:[number]=JSON.parse('[1,{"2":2,"1":1},-0]');
 if(JSON.stringify(extra)!=='[1,{"1":1,"2":2},0]')return 3;
 const result:Result<number>=JSON.parse('{"extra":true,"val":7,"tag":"ok"}');
 if(JSON.stringify(result)!=='{"tag":"ok","val":7,"extra":true}')return 4;
 const empty:{tag:'empty'}|{tag:'value';val:number}=JSON.parse('{"val":{"n":2},"tag":"empty","extra":7}');
 if(JSON.stringify(empty)!=='{"tag":"empty","val":{"n":2},"extra":7}')return 5;
 return 0;
}
export function warm():number {
 saved=JSON.parse('{    "one": {"id":1, "values":[1, 2], "extra":"é🙂"},    "two": {"id":2, "values":[4, 5], "extra":{"2":2,"1":1}}    }');
 return JSON.stringify(saved).length;
}
export function read():number {
 const doc=saved;if(doc===undefined)return -1;
 return JSON.stringify(doc.two)==='{"id":2,"values":[4,5],"extra":{"1":1,"2":2}}'?1:0;
}
export function mutate():number {
 const doc=saved;if(doc===undefined)return -1;
 doc.two.values.push(6);const alias=doc.two;alias.id=9;
 return JSON.stringify(doc.two)==='{"id":9,"values":[4,5,6],"extra":{"1":1,"2":2}}'?1:0;
}
"#;
    let mut program = Program::checked(source, "typed-serialization.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        assert_eq!(program.outcome("members", &[]), (0.0, 0));
        assert_eq!(program.outcome("tuples", &[]), (0.0, 0));
        assert!(program.outcome("warm", &[]).0 > 0.0);
        assert_eq!(program.outcome("read", &[]), (1.0, 0));
        assert_eq!(program.outcome("mutate", &[]), (1.0, 0));
    }
}

/// Typed records and dynamic values keep their parsed strings across suspension, failed
/// parses and later exports, while replaced documents and unused input become collectible.
#[test]
fn parsed_json_survives_suspension_and_releases_replaced_documents() {
    let source = r#"
interface Row {name:string;n:number}
let row:Row={name:'initial',n:0};
let rowView='';
let saved=JSON.parse('null');
let view='';
export async function run(n:number):Promise<number> {
 if(n>0 && (row.name!=='Café 🙂' || rowView!=='escaped 🙂' || saved.text!=='Café 🙂' || view!=='escaped 🙂'))return 1;
 const rows:Row[]=JSON.parse('[{"name":"Café 🙂","n":1,"extra":true},{"name":"escaped \\ud83d\\ude42","n":2}]');
 row=rows[0];
 rowView=rows[1].name;
 let input='{"text":"Café 🙂","nested":[{"value":"escaped \\ud83d\\ude42"}],"n":'+n+'}';
 saved=JSON.parse(input);
 view=saved.nested[0].value as string;
 input+='overwritten input';
 await Promise.resolve(n);
 for(let i=0;i<80;i++) {
  const temporary:Row=JSON.parse('{"name":"temporary","n":3}');
  try {const bad:Row=JSON.parse('{"name":false,"n":3,"skip":[1,]}');}catch(e){}
  try {JSON.parse('{"bad":[1,]}');}catch(e){}
  if(temporary.n!==3)return 2;
 }
 if(JSON.stringify(row)!=='{"name":"Café 🙂","n":1,"extra":true}' || saved.n!==n)return 3;
 return row.name.length+rowView.length;
}
export function scalar(n:number):number {
 let input='';
 for(let i=0;i<1024;i++)input+='                                ';
 saved=JSON.parse(input+(n===0?'42':n===1?'"\\u00e9"':'""'));
 view='';
 if(n===0 && saved!==42 || n===1 && saved!=='é' || n===2 && saved!=='')return 1;
 return 0;
}
"#;
    let mut program = Program::checked(source, "retained-json.ts");
    program.stress_collection(true);
    let mut live = None;
    for n in 0..4 {
        assert_eq!(program.throwing("run", f64::from(n)), (15.0, 0), "run {n}");
        let bytes = program.counter(Counter::LiveBytes);
        if let Some(previous) = live {
            assert_eq!(bytes, previous, "replaced documents must be collectible");
        }
        live = Some(bytes);
    }
    for n in 0..3 {
        assert_eq!(program.throwing("scalar", f64::from(n)), (0.0, 0));
        assert!(
            program.counter(Counter::LiveBytes) < 4096,
            "scalar graphs must release unused input storage"
        );
    }
    assert_eq!(program.counter(Counter::Roots), 0);
    assert_eq!(program.counter(Counter::Pins), 0);
    assert!(program.memory_bytes() <= 1024 * 1024);
}

#[test]
fn typed_json_fuel_and_allocations_scale_linearly() {
    // Bound graph parsing by main's resource use and retain the linear scaling gate.
    let cases = [(256, 5_500_000, 530), (1024, 21_000_000, 2100)];
    let mut fuel = Vec::new();
    for (count, fuel_limit, baseline_allocations) in cases {
        let source = row_benchmark(count);
        let mut program = Program::from_wasm(
            perry_wit::compile_core(&source, "json-bench.ts").expect("source compiles"),
        );
        // Measure the steady state after one warm-up call.
        program.call("run", &[]);
        let (_, used) = program.call("run", &[]);
        assert!(
            used < fuel_limit,
            "graph decoder fuel regression: {used} for {count} rows"
        );
        let mut checked = Program::checked(&source, "json-bench.ts");
        checked.call("run", &[]);
        let allocations = checked.counter(Counter::Allocations);
        assert!(
            allocations <= baseline_allocations,
            "graph decoder allocation regression: {allocations} for {count} rows"
        );
        assert_eq!(checked.counter(Counter::LiveBytes), 0);
        fuel.push(used);
    }
    assert!(
        fuel[1] * 100 < fuel[0] * 425,
        "quadrupling JSON input must scale linearly: {fuel:?}"
    );
}

/// Source of the typed `Row[]` decode benchmark over `count` rows, each carrying an unknown
/// nested field the decoder must skip; `run` returns the sum of ids and name lengths.
fn row_benchmark(count: u32) -> String {
    let text = serde_json::to_string(
        &(0..count)
            .map(|i| {
                serde_json::json!({
                    "id": i, "name": "Café 🙂", "price": 12.5,
                    "unknown": {"nested": [true, false, null, "discarded"]}
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    format!(
        r#"
interface Row {{id:number;name:string;price:number}}
export function run():number {{
  const rows:Row[] = JSON.parse({});
  let sum = 0;
  for (let i=0;i<rows.length;i++) sum += rows[i].id + rows[i].name.length;
  return sum;
}}
"#,
        serde_json::to_string(&text).unwrap()
    )
}

/// Repeated field writes, number serialization and string quoting allocate a bounded amount
/// independent of their count, and a finished JSON string owns its buffer after the call.
#[test]
fn serialization_and_field_writes_keep_bounded_allocations() {
    let text = format!(
        "é漢🙂\"\\{}tail",
        (0..32).map(char::from).collect::<String>()
    );
    let quoted = serde_json::to_string(&serde_json::to_string(&text).unwrap()).unwrap();
    let source = format!(
        r#"
interface Item {{count:number; label:string; added?:string}}
let label='';
let saved='';
export function writes(n:number):number {{
 const item:Item=JSON.parse('{{ "count": 1, "unknown": {{"2":2,"1":1}}, "label":"é🙂" }}');
 label=item.label;
 const before=JSON.stringify(item);
 if(before!==JSON.stringify(item))return -1;
 for(let i=0;i<n;i++){{item.count++;item.added='yes';}}
 if(JSON.stringify(item)!=='{{"count":'+String(n+1)+',"unknown":{{"1":1,"2":2}},"label":"é🙂","added":"yes"}}')return -2;
 return item.count;
}}
export function churn(n:number):number {{
 const item:Item=JSON.parse('{{"count":1,"unknown":true,"label":"é🙂","added":"before"}}');
 const copy={{...item}};
 for(let i=0;i<n;i++){{delete item.added;item.added=item.label;}}
 if(JSON.stringify(copy)!=='{{"count":1,"unknown":true,"label":"é🙂","added":"before"}}')return -1;
 return item.count+(item.added===item.label?1:0);
}}
export function numbers(n:number):number {{
 const values:number[]=[];
 for(let i=0;i<n;i++)values.push(i);
 const text=JSON.stringify(values);
 return text[0]==='[' && text[text.length-1]===']' ? n : -1;
}}
export function quote():number {{saved=JSON.stringify({text});return saved==={quoted}?1:0;}}
export function retained():number {{return saved==={quoted} && label==='é🙂'?1:0;}}
"#,
        text = serde_json::to_string(&text).unwrap()
    );
    let mut program = Program::checked(&source, "bounded-serialization.ts");
    // Allocations of one call of `export` with `n`, which returns `expected`.
    let mut allocations = |export: &str, n: f64, expected: f64| {
        let before = program.counter(Counter::Allocations);
        assert_eq!(program.number(export, n), expected);
        program.counter(Counter::Allocations) - before
    };
    let (small, large) = (
        allocations("writes", 10.0, 11.0),
        allocations("writes", 1000.0, 1001.0),
    );
    assert!(
        large <= small + 8,
        "field writes allocated {large} versus {small}"
    );
    let (small, large) = (
        allocations("numbers", 16.0, 16.0),
        allocations("numbers", 1024.0, 1024.0),
    );
    assert!(
        large < small + 64,
        "numbers allocated {large} versus {small}"
    );
    program.stress_collection(true);
    assert_eq!(program.number("writes", 100.0), 101.0);
    assert_eq!(program.number("churn", 8192.0), 2.0);
    assert!(
        program.memory_bytes() <= 1024 * 1024,
        "property-order churn must remain collectible"
    );
    assert_eq!(program.call("quote", &[]).0[0].unwrap_f64(), 1.0);
    assert_eq!(program.number("numbers", 1024.0), 1024.0);
    assert_eq!(program.call("retained", &[]).0[0].unwrap_f64(), 1.0);
}

#[test]
fn graph_serialization_allocations_scale_with_output() {
    let mut allocations = Vec::new();
    for count in [16, 88] {
        let row: serde_json::Value = serde_json::from_str(&format!(
            "{{\"n\":1,{}}}",
            (0..53)
                .map(|i| format!("\"extra{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        ))
        .unwrap();
        let rows = vec![row.clone(); count];
        let input = serde_json::to_string(&serde_json::to_string_pretty(&rows).unwrap()).unwrap();
        let expected = serde_json::to_string(&row).unwrap().len() * count;
        let source = format!(
            r#"
export function parse():number {{ const rows=JSON.parse({input});return rows.length; }}
export function write():number {{ const rows=JSON.parse({input});let size=0;for(let i=0;i<rows.length;i++)size+=JSON.stringify(rows[i]).length;return size; }}
"#
        );
        // Two instances of one module: the difference is what serialization allocates.
        let mut parse = Program::checked(&source, "serialization-allocations.ts");
        let mut write = Program::from_wasm(parse.wasm.clone());
        let (parsed, parse_fuel) = parse.call("parse", &[]);
        let (written, write_fuel) = write.call("write", &[]);
        assert_eq!(parsed[0].unwrap_f64(), count as f64);
        assert_eq!(written[0].unwrap_f64(), expected as f64);
        assert_eq!(write.counter(Counter::LiveBytes), 0);
        let serialization_fuel = write_fuel - parse_fuel;
        assert!(
            serialization_fuel < count as u64 * 200_000,
            "54-field serialization fuel regression: {serialization_fuel} for {count} rows"
        );
        allocations.push(write.counter(Counter::Allocations) - parse.counter(Counter::Allocations));
    }
    // 5.5 times the rows may cost at most 6 times the allocations.
    assert!(
        allocations[1] <= allocations[0] * 6,
        "serialization allocations grew from {} to {}",
        allocations[0],
        allocations[1]
    );
}

#[test]
fn immutable_json_lookup_preserves_duplicates_keys_and_copied_container_owners() {
    let members = (0..40)
        .map(|i| format!("\"k{i}\":{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let values = (0..128)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let input = format!("{{{members},\"k0\":99,\"\\u006b1\":88,\"é🙂\":17,\"values\":[{values}]}}");
    let literal = serde_json::to_string(&input).unwrap();
    let source = format!(
        r#"
let kept=JSON.parse('{{}}');
export function prepare():number {{
 const original=JSON.parse({literal});
 const copy={{...original,k0:100}};
 if(original.k0!==99 || original.k1!==88 || copy.k0!==100 || copy.k1!==88)return 1;
 kept=copy;
 return 0;
}}
export function read():number {{
 if(kept.k0!==100 || kept.k1!==88 || kept['é🙂']!==17 || kept.missing!==undefined)return 1;
 for(let i=2;i<40;i++)if(kept['k'+i]!==i)return 2;
 const values=kept.values;
 for(let i=0;i<128;i++)if(values[i]!==i)return 3;
 if(values[128]!==undefined || values[-1]!==undefined || values[1.5]!==undefined)return 4;
 return 0;
}}
"#
    );
    let mut program = Program::checked(&source, "indexed-json.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        assert_eq!(program.outcome("prepare", &[]), (0.0, 0));
        assert_eq!(program.outcome("read", &[]), (0.0, 0));
    }
    assert_eq!(program.counter(Counter::Roots), 0);
    assert_eq!(program.counter(Counter::Pins), 0);
}
