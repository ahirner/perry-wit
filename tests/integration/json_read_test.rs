//! Typed `JSON.parse` decoding (validation, duplicates, variants, limits) and source-backed
//! `JSON.stringify` of decoded values, with their allocation and fuel bounds.
use crate::core_program::{Counter, Program};

#[test]
fn compiled_and_parsed_keys_use_canonical_escapes() {
    let mut source = String::from("export function run():number {\n");
    for key in (0..32).map(char::from).map(|c| c.to_string()).chain([
        "\"\\".to_string(),
        "é🙂".to_string(),
        "x".repeat(260),
    ]) {
        let literal = serde_json::to_string(&key).unwrap();
        let object = format!("{{{literal}:7}}");
        let expected = serde_json::to_string(&object).unwrap();
        source.push_str(&format!(
            "if(JSON.stringify({object}) !== {expected}) return 1;\n\
             if(JSON.stringify(JSON.parse({expected})) !== {expected}) return 2;\n"
        ));
    }
    source.push_str("return 0;}");
    let mut program = Program::checked(&source, "quoted-keys.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn parsed_views_survive_suspension_errors_and_repeated_exports() {
    let source = r#"
interface Row {name:string;n:number}
let saved:Row={name:'initial',n:0};
let view='';
export async function run(n:number):Promise<number> {
 if(n>0 && (saved.name!=='Café 🙂' || view!=='escaped 🙂'))return 1;
 const rows:Row[]=JSON.parse('[{"name":"Café 🙂","n":1,"extra":true},{"name":"escaped \\ud83d\\ude42","n":2}]');
 saved=rows[0];
 view=rows[1].name;
 await Promise.resolve(n);
 for(let i=0;i<80;i++) {
  const temporary:Row=JSON.parse('{"name":"temporary","n":3}');
  try {const bad:Row=JSON.parse('{"name":false,"n":3,"skip":[1,]}');}catch(e){}
  if(temporary.n!==3)return 2;
 }
 if(JSON.stringify(saved)!=='{"name":"Café 🙂","n":1,"extra":true}')return 3;
 return saved.name.length+view.length;
}
"#;
    let mut program = Program::checked(source, "retained-json.ts");
    program.stress_collection(true);
    let mut live = None;
    for n in 0..4 {
        assert_eq!(program.throwing("run", f64::from(n)), (15.0, 0));
        let bytes = program.counter(Counter::LiveBytes);
        if let Some(previous) = live {
            assert_eq!(bytes, previous, "old documents must be collectible");
        }
        live = Some(bytes);
        assert_eq!(program.counter(Counter::Roots), 0);
        assert_eq!(program.counter(Counter::Pins), 0);
    }
    assert!(program.memory_bytes() <= 1024 * 1024);
}

#[test]
fn tuple_spans_and_union_members_keep_runtime_order() {
    let source = r#"
type Result<T>={tag:'ok';val:T}|{tag:'err';val:string};
export function run():number {
 const tuple:[number,string|undefined]=JSON.parse('[1]');
 if(JSON.stringify(tuple)!=='[1]')return 1;
 tuple[1]='added';
 if(JSON.stringify(tuple)!=='[1,"added"]')return 2;
 const extra:[number]=JSON.parse('[1,{"2":2,"1":1},-0]');
 if(JSON.stringify(extra)!=='[1,{"1":1,"2":2},0]')return 3;
 const result:Result<number>=JSON.parse('{"extra":true,"val":7,"tag":"ok"}');
 if(JSON.stringify(result)!=='{"extra":true,"val":7,"tag":"ok"}')return 4;
 const canonical:{n:number}=JSON.parse('{"n":1}');
 if(JSON.stringify(canonical)!=='{"n":1}' || JSON.stringify(canonical)!=='{"n":1}')return 5;
 canonical.n=2;
 if(JSON.stringify(canonical)!=='{"n":2}')return 6;
 return 0;
}
"#;
    let mut program = Program::checked(source, "tuple-source.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn duplicate_members_validate_only_the_final_typed_value() {
    let source = r#"
type Result<T> = {tag:'ok';val:T}|{tag:'err';val:string};
interface Item {n:number}
interface Envelope {result:Result<Item>}
interface Row {name:string; inner:Item; extra?:string}
export function run():number {
 const tagged:Envelope=JSON.parse('{"result":{"val":{"n":4},"tag":"ok"}}');
 if(tagged.result.tag !== 'ok') return 1;
 if(tagged.result.val.n !== 4) return 2;
 const duplicate:Envelope=JSON.parse('{"result":{"val":false,"tag":false,"tag":"ok","val":{"n":5}}}');
 if(duplicate.result.tag !== 'ok') return 3;
 if(duplicate.result.val.n !== 5) return 4;
 const overwritten:Envelope=JSON.parse('{"result":{"tag":"missing"},"result":{"tag":"err","val":"yes"}}');
 if(overwritten.result.tag !== 'err') return 5;
 if(overwritten.result.val !== 'yes') return 6;
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"missing"},"ignored":[1,]}');return 7;}
 catch(e) {if(!e.message.includes('SyntaxError')) return 8;}
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"ok"}}');return 9;} catch(e) {}
 const row:Row = JSON.parse('{"name":false,"inner":{"n":false},"name":"é🙂","inner":{"n":3}}');
 if(row.name.length !== 2 || row.inner.n !== 3 || row.extra !== undefined) return 10;
 try {const bad:Row = JSON.parse('{"name":"ok","inner":{"n":1},"name":false}'); return 11;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":false,"inner":{},"ignored":[1,]}'); return 12;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok","inner":null}'); return 13;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok"}'); return 14;} catch(e) {}
 const fixed:Row = JSON.parse('{"name":"ok","inner":false,"inner":{"n":4}}');
 return fixed.inner.n - 4;
}
"#;
    let mut program = Program::checked(source, "duplicates.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn typed_json_validates_depth_skipped_values_and_numeric_extremes() {
    let nested = |depth: usize| format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    let good = serde_json::to_string(&format!(
        r#"{{"n":-0,"text":"é🙂","ignored":{}}}"#,
        nested(127)
    ))
    .unwrap();
    let bad = serde_json::to_string(&format!(r#"{{"n":0,"text":"","ignored":{}}}"#, nested(128)))
        .unwrap();
    let source = format!(
        r#"
interface Row {{n:number;text:string}}
export function run():number {{
 const row:Row=JSON.parse({good});
 if(1/row.n !== -Infinity || row.text.length !== 2) return 1;
 try {{const bad:Row=JSON.parse({bad});return 2;}}catch(e){{}}
 const over:Row=JSON.parse('{{"n":1e400,"text":"\\u00e9\\ud83d\\ude42"}}');
 if(over.n !== Infinity || over.text !== row.text) return 3;
 try {{const bad:Row=JSON.parse('{{"n":0,"text":"\\ud800"}}');return 4;}}catch(e){{}}
 try {{const bad:Row=JSON.parse('{{"n":0,"text":""}} true');return 5;}}catch(e){{}}
 const escaped:Row=JSON.parse('{{"\\u006e":7,"text":"ok"}}');
 return escaped.n-7;
}}
"#
    );
    assert_eq!(
        Program::checked(&source, "limits.ts").outcome("run", &[]),
        (0.0, 0)
    );
}

#[test]
fn source_backed_stringify_preserves_order_extras_and_mutations() {
    let source = r#"
interface Inner {n:number}
interface Row {name:string; inner:Inner; values:number[]; added?:number; another?:number}
export function run():number {
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
 return 0;
}
"#;
    let mut program = Program::checked(source, "provenance.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        assert_eq!(program.outcome("run", &[]), (0.0, 0));
    }
}

#[test]
fn typed_json_fuel_and_allocations_scale_linearly() {
    // Preserve pre-cleanup typed parsing fuel; allocations stay below the graph baseline.
    let cases = [(256, 1_730_000, 521), (1024, 6_900_000, 2059)];
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
            "typed decoder fuel regression: {used} for {count} rows"
        );
        let mut checked = Program::checked(&source, "json-bench.ts");
        checked.call("run", &[]);
        let allocations = checked.counter(Counter::Allocations);
        assert!(
            allocations < baseline_allocations * 3 / 4,
            "typed decoder allocation regression: {allocations} for {count} rows"
        );
        assert_eq!(checked.counter(Counter::LiveBytes), 0);
        fuel.push(used);
    }
    assert!(
        fuel[1] * 100 < fuel[0] * 425,
        "quadrupling JSON input must scale linearly: {fuel:?}"
    );
}

/// Opt-in benchmark of the release CLI: prints compile cost, fuel, time and memory per input
/// size, and writes them to `PERRY_JSON_BENCH_REPORT` when that names a file.
#[test]
#[ignore = "release CLI compilation and linear memory comparison"]
fn typed_json_release_compiler_baseline() {
    let compiler = std::env::var("PERRY_JSON_BENCH_COMPILER").expect("release compiler path");
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.ts");
    let core = directory.path().join("core.wasm");
    let component = directory.path().join("component.wasm");
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:baseline; world run { export run: func() -> f64; }",
    )
    .unwrap();
    let mut rows = Vec::new();
    for count in [256, 1024] {
        std::fs::write(&input, row_benchmark(count)).unwrap();
        let mut costs = Vec::new();
        for sample in 0..5 {
            let started = std::time::Instant::now();
            let mut command = std::process::Command::new(&compiler);
            command
                .arg(&input)
                .arg("--wit")
                .arg(directory.path())
                .arg("-o")
                .arg(&component);
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            costs.push(started.elapsed().as_secs_f64() * 1000.0);
            if sample == 0 {
                let output = std::process::Command::new(&compiler)
                    .arg(&input)
                    .arg("--wit")
                    .arg(directory.path())
                    .arg("--core-only")
                    .arg("-o")
                    .arg(&core)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        costs.sort_by(f64::total_cmp);
        let wasm = std::fs::read(&core).unwrap();
        let core_bytes = wasm.len();
        let mut program = Program::from_wasm(wasm);
        let mut times = Vec::new();
        let mut fuel = 0;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let (result, used) = program.call("run", &[]);
            assert_eq!(
                result[0].unwrap_f64(),
                f64::from(count * (count - 1) / 2 + count * 6)
            );
            fuel = used;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
            program.call("cabi_post_run", &result);
        }
        times.sort_by(f64::total_cmp);
        rows.push(serde_json::json!({
            "rows": count,
            "component_bytes": std::fs::metadata(&component).unwrap().len(),
            "core_bytes": core_bytes,
            "compile_median_ms": costs[2],
            "fuel": fuel,
            "median_ms": times[2],
            "peak_linear_bytes": program.memory_bytes(),
        }));
    }
    let report = serde_json::to_string_pretty(&rows).unwrap();
    println!("{report}");
    if let Ok(path) = std::env::var("PERRY_JSON_BENCH_REPORT") {
        std::fs::write(path, report).unwrap();
    }
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

#[test]
fn repeated_field_writes_and_serialization_keep_bounded_work() {
    let source = r#"
interface Item {count:number; label:string; added?:string}
let retained='';
export function run(n:number):number {
 const item:Item=JSON.parse('{ "count": 1, "unknown": {"2":2,"1":1}, "label":"é🙂" }');
 retained=item.label;
 const before=JSON.stringify(item);
 if(before!==JSON.stringify(item))return -1;
 for(let i=0;i<n;i++){item.count++;item.added='yes';}
 if(JSON.stringify(item)!=='{"count":'+String(n+1)+',"unknown":{"1":1,"2":2},"label":"é🙂","added":"yes"}')return -2;
 return item.count;
}
export function saved():number {return retained==='é🙂'?1:0;}
"#;
    let mut program = Program::checked(source, "json-assignment.ts");
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("run", 10.0), 11.0);
    let small = program.counter(Counter::Allocations) - before;
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("run", 1000.0), 1001.0);
    let large = program.counter(Counter::Allocations) - before;
    assert!(
        large <= small + 8,
        "repeated writes allocated {large} versus {small}"
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 100.0), 101.0);
    assert_eq!(program.call("saved", &[]).0[0].unwrap_f64(), 1.0);
}

#[test]
fn source_indexes_resolve_collisions_duplicates_and_sort_boundaries() {
    // Member counts around the source index's sorting and hashing thresholds.
    let counts = [0, 1, 16, 28, 29, 32, 33, 54, 80, 128, 129];
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for count in counts {
        let mut fields: Vec<_> = (0..count).map(|i| format!("\"key{i}\":{i}")).collect();
        if count > 0 {
            fields.push("\"\\u006bey0\":9".into());
        }
        fields.extend([
            "\"2\":2".into(),
            "\"1\":1".into(),
            "\"n\":7".into(),
            "\"\\u006e\":8".into(),
        ]);
        inputs.push(format!("{{{}}}", fields.join(",")));
        let mut expected = vec!["\"1\":1".to_string(), "\"2\":2".into()];
        expected.extend((0..count).map(|i| format!("\"key{i}\":{}", if i == 0 { 9 } else { i })));
        expected.push("\"n\":8".into());
        outputs.push(format!("{{{}}}", expected.join(",")));
    }
    let source = format!(
        r#"
const inputs:string[]={};
const outputs:string[]={};
export function run():number {{
 for(let i=0;i<inputs.length;i++) {{
  const value:{{n:number;key0?:number}}=JSON.parse(inputs[i]);
  if(JSON.stringify(value)!==outputs[i]) return i+1;
 }}
 return 0;
}}
"#,
        serde_json::to_string(&inputs).unwrap(),
        serde_json::to_string(&outputs).unwrap()
    );
    let mut program = Program::checked(&source, "source-key-index.ts");
    program.stress_collection(true);
    let (failed, status) = program.outcome("run", &[]);
    assert_eq!(status, 0);
    assert_eq!(
        failed,
        0.0,
        "the case with {:?} members",
        counts.get(failed as usize - 1)
    );
}

#[test]
fn serialization_transfers_buffers_and_avoids_per_number_allocations() {
    let controls: String = (0..32).map(char::from).collect();
    let texts: Vec<_> = (0..17)
        .map(|prefix| format!("{}é漢🙂\"\\{controls}tail", "x".repeat(prefix)))
        .collect();
    let quoted: Vec<_> = texts
        .iter()
        .map(|text| serde_json::to_string(text).unwrap())
        .collect();
    let source = format!(
        r#"
let saved = '';
export function strings():number {{
 const texts:string[]={}; const quoted:string[]={};
 for(let i=0;i<texts.length;i++){{
   const value=JSON.stringify(texts[i]);
   if(value!==quoted[i])return -1;
   if(i===0)saved=value;
 }}
 return saved===quoted[0]?1:0;
}}
export function numbers(n:number):number {{
 const values:number[]=[];
 for(let i=0;i<n;i++)values.push(i);
 const text=JSON.stringify(values);
 if(text[0]!=='[' || text[text.length-1]!==']')return -1;
 if(JSON.stringify([-0,Infinity,-Infinity,NaN,1e-7,1e21])!=='[0,null,null,null,1e-7,1e+21]')return -2;
 return n;
}}
export function retained():number{{return saved==={}?1:0;}}
"#,
        serde_json::to_string(&texts).unwrap(),
        serde_json::to_string(&quoted).unwrap(),
        serde_json::to_string(&quoted[0]).unwrap()
    );
    let mut program = Program::checked(&source, "json-writer-storage.ts");
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("numbers", 16.0), 16.0);
    let small = program.counter(Counter::Allocations) - before;
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("numbers", 1024.0), 1024.0);
    let large = program.counter(Counter::Allocations) - before;
    assert!(
        large < small + 64,
        "number serialization allocations grew from {small} to {large}"
    );
    program.stress_collection(true);
    assert_eq!(program.call("strings", &[]).0[0].unwrap_f64(), 1.0);
    assert_eq!(program.number("numbers", 1024.0), 1024.0);
    assert_eq!(program.call("retained", &[]).0[0].unwrap_f64(), 1.0);
}

#[test]
fn cached_roots_preserve_nesting_when_serialized_as_subtrees() {
    let nested = format!("{}0{}", "[".repeat(127), "]".repeat(127));
    let text = serde_json::to_string(&format!(r#"{{"n":1,"extra":{nested}}}"#)).unwrap();
    let source = format!(
        r#"
interface Item {{n:number}}
export function run():number {{
 const row:Item=JSON.parse({text});
 const cached=JSON.stringify(row);
 if(JSON.stringify(row)!==cached)return 1;
 try {{JSON.stringify([row]);return 2;}}catch(e){{}}
 const short:Item=JSON.parse('{{ "n": 2, "extra": [1] }}');
 const normalized=JSON.stringify(short);
 if(JSON.stringify([short])!=='['+normalized+']')return 3;
 short.n=3;
 if(JSON.stringify([short])!=='[{{"n":3,"extra":[1]}}]')return 4;
 return 0;
}}
"#
    );
    let mut program = Program::checked(&source, "cached-subtree.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn source_serialization_stack_handles_mixed_depth_and_siblings() {
    let mut nested = "0".to_string();
    for depth in 0..127 {
        nested = if depth % 2 == 0 {
            format!("[false,{nested},null]")
        } else {
            format!(r#"{{"1":1,"next":{nested},"tail":"é🙂"}}"#)
        };
    }
    let object = format!(r#"{{"n":1,"before":[],"deep":{nested},"after":{{"1":1,"2":2}}}}"#);
    let text = serde_json::to_string(&object).unwrap();
    let changed = serde_json::to_string(&object.replacen(r#""n":1"#, r#""n":2"#, 1)).unwrap();
    let source = format!(
        r#"
interface Row {{n:number}}
export function run():number {{
 const row:Row=JSON.parse({text});
 try {{JSON.stringify([row]);return 1;}}catch(e){{}}
 if(JSON.stringify(row)!=={text})return 2;
 row.n=2;
 if(JSON.stringify(row)!=={changed})return 3;
 const next:Row=JSON.parse('{{"n":3,"tail":[{{"2":2,"1":1}},false]}}');
 if(JSON.stringify(next)!=='{{"n":3,"tail":[{{"1":1,"2":2}},false]}}')return 4;
 return 0;
}}
"#
    );
    let mut program = Program::checked(&source, "source-stack.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        assert_eq!(program.outcome("run", &[]), (0.0, 0));
        assert_eq!(program.counter(Counter::LiveBytes), 0);
    }
}

#[test]
fn typed_key_tables_handle_unicode_and_overwritten_errors() {
    let mut keys: Vec<_> = (0..40).map(|index| format!("field{index}")).collect();
    keys.extend(["é🙂".into(), "".into(), "x".repeat(260)]);
    let fields = keys
        .iter()
        .map(|key| format!("{}:number", serde_json::to_string(key).unwrap()))
        .collect::<Vec<_>>()
        .join(";");
    let mut members: Vec<_> = keys
        .iter()
        .enumerate()
        .rev()
        .map(|(index, key)| format!("{}:{index}", serde_json::to_string(key).unwrap()))
        .collect();
    members.insert(0, "\"field0\":false".into());
    members.push("\"ignored\":{\"field0\":false}".into());
    members.push("\"\\u0066ield1\":17".into());
    let input = serde_json::to_string(&format!("{{{}}}", members.join(","))).unwrap();
    let checks = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            format!(
                "if(value[{}]!=={})return 1;",
                serde_json::to_string(key).unwrap(),
                if index == 1 { 17 } else { index }
            )
        })
        .collect::<String>();
    let source = format!(
        "export function run():number {{const value:{{{fields}}}=JSON.parse({input});{checks}return 0;}}"
    );
    let mut program = Program::checked(&source, "typed-key-index.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn source_serialization_allocations_scale_with_output() {
    let mut allocations = Vec::new();
    let mut measurements = Vec::new();
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
interface Row {{n:number}}
export function parse():number {{ const rows:Row[]=JSON.parse({input});return rows.length; }}
export function write():number {{ const rows:Row[]=JSON.parse({input});let size=0;for(const row of rows)size+=JSON.stringify(row).length;return size; }}
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
            serialization_fuel < count as u64 * 150_000,
            "54-field serialization fuel regression: {serialization_fuel} for {count} rows"
        );
        allocations.push(write.counter(Counter::Allocations) - parse.counter(Counter::Allocations));
        measurements.push(serde_json::json!({
            "rows": count, "serializationFuel": serialization_fuel,
            "serializationAllocations": allocations.last().unwrap(),
            "totalAllocations": write.counter(Counter::Allocations),
            "peakLinearBytes": write.memory_bytes(),
        }));
    }
    if let Ok(path) = std::env::var("PERRY_JSON_SERIALIZE_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&measurements).unwrap()).unwrap();
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
fn source_escapes_stream_without_temporary_strings() {
    let decoded = "a\u{0}\u{1}\u{8}\u{c}\n\r\t\"\\/é🙂\u{2028}z";
    let escaped = r#"a\u0000\u0001\b\f\n\r\t\"\\\/\u00e9\ud83d\ude42\u2028z"#;
    let count = 80;
    let input = format!(
        "{{\"n\":1,{}}}",
        (0..count)
            .map(|i| format!("\"key{i}\":\"{escaped}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let expected = format!(
        "{{\"n\":1,{}}}",
        (0..count)
            .map(|i| format!("\"key{i}\":{}", serde_json::to_string(decoded).unwrap()))
            .collect::<Vec<_>>()
            .join(",")
    );
    let source = format!(
        r#"
export function run():number {{
 const value:{{n:number}}=JSON.parse({});
 return JSON.stringify(value)==={} ? 0 : 1;
}}
"#,
        serde_json::to_string(&input).unwrap(),
        serde_json::to_string(&expected).unwrap(),
    );
    let mut program = Program::checked(&source, "escaped-source.ts");
    program.stress_collection(true);
    for _ in 0..3 {
        let before = program.counter(Counter::Allocations);
        assert_eq!(program.outcome("run", &[]), (0.0, 0));
        assert!(program.counter(Counter::Allocations) - before < count);
        assert_eq!(program.counter(Counter::LiveBytes), 0);
    }
}

#[test]
fn source_number_spans_preserve_canonical_formatting() {
    let source = r#"
export function run():number {
 const value:{n:number}=JSON.parse('{"n":1,"raw":[0,-0,0.0,-0.0,1e999,1e-999,-1e999,999999999999999,-999999999999999,9999999999999999,1e20,1e21,1e-6,1e-7,1.00,-0.1]}');
 return JSON.stringify(value)==='{"n":1,"raw":[0,0,0,0,null,0,null,999999999999999,-999999999999999,10000000000000000,100000000000000000000,1e+21,0.000001,1e-7,1,-0.1]}' ? 0 : 1;
}
"#;
    let mut program = Program::checked(source, "source-numbers.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}
