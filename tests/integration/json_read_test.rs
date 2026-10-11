use crate::core_program::{Counter, Program};

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
fn tagged_payloads_rescan_late_discriminators_and_final_duplicates() {
    let source = r#"
type Result<T> = {tag:'ok';val:T}|{tag:'err';val:string};
interface Item {n:number}
interface Envelope {result:Result<Item>}
export function run():number {
 const row:Envelope=JSON.parse('{"result":{"val":{"n":4},"tag":"ok"}}');
 if(row.result.tag !== 'ok') return 1;
 if(row.result.val.n !== 4) return 2;
 const duplicate:Envelope=JSON.parse('{"result":{"val":false,"tag":false,"tag":"ok","val":{"n":5}}}');
 if(duplicate.result.tag !== 'ok') return 3;
 if(duplicate.result.val.n !== 5) return 4;
 const overwritten:Envelope=JSON.parse('{"result":{"tag":"missing"},"result":{"tag":"err","val":"yes"}}');
 if(overwritten.result.tag !== 'err') return 5;
 if(overwritten.result.val !== 'yes') return 6;
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"missing"},"ignored":[1,]}');return 7;}
 catch(e) {if(!e.message.includes('SyntaxError')) return 8;}
 try {const bad:Envelope=JSON.parse('{"result":{"tag":"ok"}}');return 9;} catch(e) {}
 return 0;
}
"#;
    let mut program = Program::checked(source, "variants.ts");
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
fn duplicate_fields_validate_only_the_final_typed_value() {
    let source = r#"
interface Inner {n:number}
interface Row {name:string; inner:Inner; extra?:string}
export function run():number {
 const row:Row = JSON.parse('{"name":false,"inner":{"n":false},"name":"é🙂","inner":{"n":3}}');
 if(row.name.length !== 2 || row.inner.n !== 3 || row.extra !== undefined) return 1;
 try {const bad:Row = JSON.parse('{"name":"ok","inner":{"n":1},"name":false}'); return 2;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":false,"inner":{},"ignored":[1,]}'); return 3;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok","inner":null}'); return 4;} catch(e) {}
 try {const bad:Row = JSON.parse('{"name":"ok"}'); return 5;} catch(e) {}
 const fixed:Row = JSON.parse('{"name":"ok","inner":false,"inner":{"n":4}}');
 return fixed.inner.n - 4;
}
"#;
    assert_eq!(
        Program::checked(source, "duplicates.ts").outcome("run", &[]),
        (0.0, 0)
    );
}

#[test]
fn generic_interfaces_aliases_and_defaults_share_concrete_shapes() {
    let source = r#"
interface Page<T> {items:T[];next:string|null}
interface Item<T=string> {value:T}
type Box<T> = {item:T};
interface Link<T> {value:T;next:Link<T>|null}
function size(page:Page<Item>):number {return page.items[0].value.length;}
function unbox(box:Box<number>):number {return box.item;}
export function run():number {
 const page:Page<Item> = {items:[{value:'hello'}],next:null};
 const link:Link<number> = {value:3,next:null};
 return size(page)+unbox({item:2})+link.value;
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (10.0, 0));
}

#[test]
fn typed_json_fuel_and_allocations_scale_linearly() {
    let mut rows = Vec::new();
    for count in [256, 1024] {
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
        let source = format!(
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
        );
        let started = std::time::Instant::now();
        let wasm = perry_wit::compile_core(&source, "json-bench.ts").unwrap();
        let compile_ms = started.elapsed().as_secs_f64() * 1000.0;
        let bytes = wasm.len();
        let started = std::time::Instant::now();
        let mut program = Program::from_wasm(wasm);
        let jit_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut times = Vec::new();
        let mut fuel = 0;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let (_, used) = program.call("run", &[]);
            fuel = used;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        let mut checked = Program::checked(&source, "json-bench.ts");
        checked.call("run", &[]);
        let baseline_fuel = if count == 256 { 5_145_844 } else { 20_639_617 };
        let baseline_allocations = if count == 256 { 521 } else { 2059 };
        assert!(
            fuel < baseline_fuel * 3 / 4,
            "typed decoder fuel regression"
        );
        assert!(
            checked.counter(Counter::Allocations) < baseline_allocations * 3 / 4,
            "typed decoder allocation regression"
        );
        assert_eq!(checked.counter(Counter::LiveBytes), 0);
        rows.push(
            serde_json::json!({"rows":count,"input_bytes":text.len(),"core_bytes":bytes,
            "fuel":fuel,"compile_ms":compile_ms,"jit_ms":jit_ms,"median_ms":times[2],
            "peak_linear_bytes":program.memory_bytes(),
            "allocations":checked.counter(Counter::Allocations),
            "live_bytes":checked.counter(Counter::LiveBytes)}),
        );
    }
    assert!(
        rows[1]["fuel"].as_u64().unwrap() * 100 < rows[0]["fuel"].as_u64().unwrap() * 425,
        "quadrupling JSON input must scale linearly"
    );
    let report = serde_json::to_string_pretty(&rows).unwrap();
    println!("{report}");
    if let Ok(path) = std::env::var("PERRY_JSON_BENCH_REPORT") {
        std::fs::write(path, report).unwrap();
    }
}

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
        let text = serde_json::to_string(&(0..count).map(|i| serde_json::json!({"id":i,"name":"Café 🙂","price":12.5,"unknown":{"nested":[true,false,null,"discarded"]}})).collect::<Vec<_>>()).unwrap();
        let source = format!(
            r#"interface Row{{id:number;name:string;price:number}}
export function run():number{{const rows:Row[]=JSON.parse({});let sum=0;for(let i=0;i<rows.length;i++)sum+=rows[i].id+rows[i].name.length;return sum;}}"#,
            serde_json::to_string(&text).unwrap()
        );
        std::fs::write(&input, source).unwrap();
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
        rows.push(serde_json::json!({"rows":count,"component_bytes":std::fs::metadata(&component).unwrap().len(),"core_bytes":core_bytes,"compile_median_ms":costs[2],"fuel":fuel,"median_ms":times[2],"peak_linear_bytes":program.memory_bytes()}));
    }
    let report = serde_json::to_string_pretty(&rows).unwrap();
    println!("{report}");
    if let Ok(path) = std::env::var("PERRY_JSON_BENCH_REPORT") {
        std::fs::write(path, report).unwrap();
    }
}

#[test]
fn repeated_field_writes_and_serialization_keep_bounded_work() {
    use crate::core_program::Counter;
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
    for count in [0, 1, 16, 28, 29, 32, 33, 80] {
        let mut fields: Vec<_> = (0..count).map(|i| format!("\"key{i}\":{i}")).collect();
        fields.extend([
            "\"2\":2".into(),
            "\"1\":1".into(),
            "\"n\":7".into(),
            "\"\\u006e\":8".into(),
        ]);
        let input = format!("{{{}}}", fields.join(","));
        let mut expected = vec!["\"1\":1".to_string(), "\"2\":2".into()];
        expected.extend((0..count).map(|i| format!("\"key{i}\":{i}")));
        expected.push("\"n\":8".into());
        let expected = format!("{{{}}}", expected.join(","));
        let source = format!(
            "export function run():number {{const value:{{n:number}}=JSON.parse({});return JSON.stringify(value)==={} ? 0:1;}}",
            serde_json::to_string(&input).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        let mut program = Program::checked(&source, "source-key-index.ts");
        program.stress_collection(true);
        assert_eq!(program.outcome("run", &[]), (0.0, 0));
    }
}

#[test]
fn serialization_transfers_buffers_and_avoids_per_number_allocations() {
    use crate::core_program::Counter;
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
fn source_member_indexes_handle_large_objects_and_duplicate_escaped_keys() {
    for count in [32, 33, 54, 128, 129] {
        let members = (0..count).map(|i| format!("\"key{i}\":{i}"));
        let text = format!(
            "{{{},\"\\u006bey0\":9,\"2\":2,\"1\":1}}",
            members.collect::<Vec<_>>().join(",")
        );
        let expected = format!(
            "{{\"1\":1,\"2\":2,{}}}",
            (0..count)
                .map(|i| format!("\"key{i}\":{}", if i == 0 { 9 } else { i }))
                .collect::<Vec<_>>()
                .join(",")
        );
        let source = format!(
            r#"
export function run():number {{
 const value:{{key0:number}}=JSON.parse({});
 return JSON.stringify(value)==={} ? 0 : 1;
}}
"#,
            serde_json::to_string(&text).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        let mut program = Program::checked(&source, "source-index.ts");
        program.stress_collection(true);
        assert_eq!(program.outcome("run", &[]), (0.0, 0), "{count} members");
    }
}

#[test]
fn typed_key_tables_handle_unicode_and_overwritten_errors() {
    let mut keys: Vec<_> = (0..40).map(|index| format!("field{index}")).collect();
    keys.extend(["é🙂".into(), "".into()]);
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
        let mut parse = Program::checked(&source, "serialization-allocations.ts");
        let mut write = Program::from_wasm(parse.wasm.clone());
        assert_eq!(parse.call("parse", &[]).0[0].unwrap_f64(), count as f64);
        let (value, fuel) = write.call("write", &[]);
        assert_eq!(value[0].unwrap_f64(), expected as f64);
        let allocations = write.counter(Counter::Allocations) - parse.counter(Counter::Allocations);
        assert_eq!(write.counter(Counter::LiveBytes), 0);
        measurements.push(serde_json::json!({"rows":count,"serialization_allocations":allocations,"total_allocations":write.counter(Counter::Allocations),"fuel":fuel,"peak_linear_bytes":write.memory_bytes()}));
    }
    assert!(
        measurements[1]["serialization_allocations"]
            .as_i64()
            .unwrap()
            <= measurements[0]["serialization_allocations"]
                .as_i64()
                .unwrap()
                * 6
    );
    println!("{}", serde_json::to_string_pretty(&measurements).unwrap());
    if let Ok(path) = std::env::var("PERRY_JSON_SERIALIZE_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&measurements).unwrap()).unwrap();
    }
}
