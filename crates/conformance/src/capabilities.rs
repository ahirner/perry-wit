//! Concrete standard API domains. Source generation is pure; adapters own all effects.
use crate::{
    execution::check_equivalence,
    registry::{Case, Contract, ExitStatus, Witness},
};
use proptest::prelude::*;

fn bytes_function(bytes: Vec<u8>, operation: &str, asynchronous: bool) -> Case {
    let prefix = if asynchronous { "async " } else { "" };
    let result = if asynchronous {
        "Promise<{value:number,trace:number[]}>"
    } else {
        "{value:number,trace:number[]}"
    };
    Case::Function {
        asynchronous,
        source: format!(
            r#"
export {prefix}function run(x:number):{result} {{
  const input=new Uint8Array({bytes});
  const trace:number[]=[];
  {operation}
  return {{value:x,trace}};
}}
"#,
            bytes = serde_json::to_string(&bytes).unwrap()
        ),
    }
}
const SET: &str = r#"
const destination=new Uint8Array(input.length+4);
destination.set(input,2);
try{destination.set(destination.subarray(1),x);trace.push(1);}catch{trace.push(2);}
for(let i=0;i<destination.length;i++){const byte=destination[i];if(byte===undefined)throw 91;trace.push(byte);}
"#;
const BYTE_VIEWS: &str = r#"
const view=input.subarray(x,-1);
trace.push(view.length);trace.push(view.byteOffset);trace.push(view.buffer===input.buffer?1:0);
const copy=input.slice(x,-1);
trace.push(copy.length);trace.push(copy.byteOffset);trace.push(copy.buffer===input.buffer?1:0);
const from=Uint8Array.from(input);
const converted=new Uint8Array([x,x+1,-x]);
trace.push(converted[0]);trace.push(converted[1]);trace.push(converted[2]);
const absent=input[x];trace.push(absent===undefined?-1:absent);
input[x]=x;
if(view.length>0)view[0]=17;
if(copy.length>0)copy[0]=29;
try{const bounded=new Uint8Array(input.buffer,x,0);trace.push(bounded.byteOffset);}
catch(e){trace.push(e instanceof RangeError?-2:-3);}
try{const sized=new Uint8Array(input.buffer,0,x);trace.push(sized.length);}
catch(e){trace.push(e instanceof RangeError?-2:-3);}
try{input.set([x,x+1],x);trace.push(1);}
catch(e){trace.push(e instanceof RangeError?-2:-3);}
const missing:number[]=[];
const empty=new Uint8Array(missing[0]);trace.push(empty.length);
const assigned=(input[0]=missing[0]);trace.push(assigned===undefined?1:0);
trace.push((input[0]=257.9));
const tail=input.subarray(1,missing[0]);trace.push(tail.length);
const nanEnd=input.subarray(1,0/0);trace.push(nanEnd.length);
for(let i=0;i<input.length;i++){trace.push(input[i]);trace.push(from[i]);}
for(let i=0;i<copy.length;i++)trace.push(copy[i]);
"#;
const RESPONSE: &str = r#"
const response=new Response(input,{status:201,headers:{'x-test':'bytes'}});
if(response.status!==201||response.headers.get('x-test')!=='bytes'||response.bodyUsed)throw 90;
for(let i=0;i<input.length;i++)input[i]=99;
const body=await response.bytes();
if(!response.bodyUsed)throw 91;
let rejected=false;try{await response.bytes();}catch{rejected=true;}if(!rejected)throw 92;
for(let i=0;i<body.length;i++){const byte=body[i];if(byte===undefined)throw 93;trace.push(byte);}
"#;
const REQUEST: &str = r#"
const request=new Request('https://example.test/path',{method:'POST',body:input});
for(let i=0;i<input.length;i++)input[i]=99;
if(request.method!=='POST'||request.bodyUsed)throw 90;
const body=await request.bytes();
if(!request.bodyUsed)throw 91;
let rejected=false;try{await request.bytes();}catch{rejected=true;}if(!rejected)throw 92;
for(let i=0;i<body.length;i++){const byte=body[i];if(byte===undefined)throw 93;trace.push(byte);}
"#;
fn writer(bytes: Vec<u8>) -> Case {
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
import {{Writable}} from 'node:stream';
const input=new Uint8Array({bytes});
const stream=Writable.toWeb(process.stdout);
const writer=stream.getWriter();
let locked=false;try{{stream.getWriter();}}catch{{locked=true;}}if(!locked)throw 90;
const first=writer.write(input);
const second=writer.write(input.subarray(1));
await first;await second;await first;
await writer.close();writer.releaseLock();
let closed=false;
try{{await writer.write(input);}}catch{{closed=true;}}
await Writable.toWeb(process.stdout).getWriter().write(input);
if(!closed)throw 91;
const errors=Writable.toWeb(process.stderr).getWriter();
await errors.write(input);await errors.close();errors.releaseLock();
"#,
            bytes = serde_json::to_string(&bytes).unwrap()
        ),
    }
}
pub fn contracts() -> Vec<Contract> {
    let mut contracts = vec![
        Contract {
            id: "ecma.object.enumeration",
            description: "Values and entries preserve own-key order, scalar snapshots and aggregate aliases",
            specification: "https://tc39.es/ecma262/#sec-enumerableownproperties",
            domain: "Native/parsed records, dictionaries, dense arrays, typed tuples, byte views and BMP strings; optional values, numeric keys, snapshot mutation and aggregate identities; null/undefined and stale-view errors plus surrogate-splitting rejection have separate component assertions",
            witnesses: vec![
                Witness {
                    partition: "empty-and-absent",
                    case: enumeration(0, false, String::new(), vec![]),
                },
                Witness {
                    partition: "unicode-and-present",
                    case: enumeration(-3, true, "é\0水".into(), vec![2, -1, 255]),
                },
            ],
            strategy: (
                -32i16..=32,
                any::<bool>(),
                prop::collection::vec(
                    prop::sample::select(vec!['a', 'é', '水', '\0', '"', '\\']),
                    0..16,
                ),
                prop::collection::vec(-20i16..=300, 0..16),
            )
                .prop_map(|(number, present, text, values)| {
                    enumeration(number, present, text.into_iter().collect(), values)
                })
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.object.optional_record",
            description: "Optional record fields preserve own presence, property order and parsed/native aliases",
            specification: "https://www.typescriptlang.org/docs/handbook/2/objects.html#optional-properties",
            domain: "Closed records with required and optional scalar/array/record/byte-view fields, recursive records, valid parsed views, insertion/deletion and key snapshots; native required-field protection and stale incompatible views use subset exception assertions separately",
            witnesses: vec![
                Witness {
                    partition: "initially-absent",
                    case: optional_record(0, false, String::new()),
                },
                Witness {
                    partition: "initially-present",
                    case: optional_record(-3, true, "é🙂".into()),
                },
            ],
            strategy: (
                -32i16..=32,
                any::<bool>(),
                prop::collection::vec(
                    prop::sample::select(vec!['a', 'é', '🙂', '\0', '"', '\\']),
                    0..16,
                ),
            )
                .prop_map(|(number, present, text)| {
                    optional_record(number, present, text.into_iter().collect())
                })
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.object.constrained_dictionary",
            description: "Named dictionary fields preserve optional presence, native storage and alias identity",
            specification: "https://www.typescriptlang.org/docs/handbook/2/objects.html#index-signatures",
            domain: "Valid string index signatures mixed with required and optional named fields, optional scalar values, JSON-compatible refinements, recursive dictionaries, growth, key/value snapshots and parsed/native aliases; invalidation and forbidden required-field deletion use separate subset exception assertions",
            witnesses: vec![
                Witness {
                    partition: "absent-optional",
                    case: constrained_dictionary(0, false, vec![]),
                },
                Witness {
                    partition: "present-undefined",
                    case: constrained_dictionary(-3, true, vec![2, -1, 4]),
                },
            ],
            strategy: (
                -32i16..=32,
                any::<bool>(),
                prop::collection::vec(-20i16..=20, 0..24),
            )
                .prop_map(|(count, present, entries)| {
                    constrained_dictionary(count, present, entries)
                })
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.replacer",
            description: "JSON function replacers preserve pre-order callbacks, mutations and serialized snapshots",
            specification: "https://tc39.es/ecma262/#sec-json.stringify",
            domain: "Synchronous inline arrows and direct function references over parsed/native JSON values; root replacement/undefined, object omission, array null substitution, alias mutation, key/length snapshots, excluded/introduced cycles and callback finalizers; no callback this, custom toJSON methods or general closures; surrogate-splitting indentation and Node fractional/NUL-gap discrepancies have separate component assertions",
            witnesses: vec![
                Witness {
                    partition: "key-order-and-replacement",
                    case: json_replacer(
                        r#"{"2":2,"drop":1,"a":{"x":-0},"replace":[3,4]}"#.into(),
                        3,
                        "2".into(),
                    ),
                },
                Witness {
                    partition: "array-omission-and-nested-mutation",
                    case: json_replacer(
                        r#"[1,{"a":[true,null]},"é🙂"]"#.into(),
                        -2,
                        r#""🙂""#.into(),
                    ),
                },
                Witness {
                    partition: "primitive-root",
                    case: json_replacer("-0.00e1".into(), 0, "0".into()),
                },
            ],
            strategy: (
                json_strategy(),
                -8i16..=8,
                prop::sample::select(vec!["0", "2.9", "10", r#""é🙂""#]),
            )
                .prop_map(|(text, amount, gap)| json_replacer(text, amount, gap.into()))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.reviver",
            description: "JSON revivers preserve post-order effects, source contexts and captured updates",
            specification: "https://tc39.es/ecma262/#sec-json.parse",
            domain: "Synchronous inline arrows and direct function references over UTF-8 JSON trees; primitive source tokens, numeric/duplicate property ordering, object deletion, root replacement, local finalizers and nested revivers; array-hole deletion remains an explicit dense-array RangeError outside the Node comparison",
            witnesses: vec![
                Witness {
                    partition: "duplicate-source-and-order",
                    case: json_reviver(
                        r#"{"2":1e2,"a":{"drop":1,"x":-0},"2":2.00,"s":"\u0061"}"#.into(),
                        3,
                    ),
                },
                Witness {
                    partition: "primitive-source",
                    case: json_reviver(" \n -0.00e+1\t".into(), 0),
                },
                Witness {
                    partition: "nested-arrays",
                    case: json_reviver(r#"[{"x":1},[],null,true,"漢🙂"]"#.into(), -2),
                },
            ],
            strategy: (json_strategy(), -8i16..=8)
                .prop_map(|(text, amount)| json_reviver(text, amount))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.stringify_options",
            description: "JSON property-list selection and formatted serialization preserve order and effects",
            specification: "https://tc39.es/ecma262/#sec-json.stringify",
            domain: "Parsed and native UTF-8 values, recursive property lists, duplicate string/number keys, ignored non-string/number entries, numeric and string indentation, excluded cycles and left-to-right argument effects; inherited prototype lookup and surrogate-splitting gaps are outside the current subset; numeric gaps in (0,1) and gaps containing NUL use separate spec-based component regressions because Node 24.21 disagrees with the specification",
            witnesses: vec![
                Witness {
                    partition: "order-and-duplicates",
                    case: json_options(
                        r#"{"2":2,"a":1,"b":[{"a":3,"2":4}],"a":5}"#.into(),
                        vec![0, 1, 2, 0, 2],
                        "2.9".into(),
                    ),
                },
                Witness {
                    partition: "empty",
                    case: json_options("[{},[],null]".into(), vec![], "10".into()),
                },
                Witness {
                    partition: "unicode-gap",
                    case: json_options(
                        r#"{"a":"漢🙂","b":true}"#.into(),
                        vec![1, 0],
                        serde_json::to_string("🙂🙂🙂🙂🙂🙂").unwrap(),
                    ),
                },
            ],
            strategy: (
                json_strategy(),
                prop::collection::vec(0u8..9, 0..18),
                prop_oneof![
                    (-4i16..15).prop_map(|n| n.to_string()),
                    prop::sample::select(vec![
                        "null",
                        "true",
                        "{}",
                        "[]",
                        "-1e400",
                        "1e400",
                        "2.9",
                        "\"\\t\"",
                        "\"é🙂\"",
                        "\"abcdefghijk\""
                    ])
                    .prop_map(str::to_owned)
                ],
            )
                .prop_map(|(text, keys, space)| json_options(text, keys, space))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.object.dictionary",
            description: "Typed dictionaries retain native values, aliases and ordered key/value snapshots",
            specification: "https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots",
            domain: "Record<string,T> and pure index signatures over numbers, strings, booleans, arrays, bytes and recursive dictionaries; missing/optional values, deletion, checked JSON views and valid alias writes",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: typed_dictionary(Vec::new(), 0),
                },
                Witness {
                    partition: "growth",
                    case: typed_dictionary((0..48).collect(), -7),
                },
                Witness {
                    partition: "negative-values",
                    case: typed_dictionary(vec![-3, 0, 7, -3], 9),
                },
            ],
            strategy: (prop::collection::vec(any::<i16>(), 0..32), any::<i16>())
                .prop_map(|(values, n)| typed_dictionary(values, n))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.dictionary",
            description: "JSON dictionary insertion, deletion, own-property presence and ordered key snapshots",
            specification: "https://tc39.es/ecma262/#sec-ordinaryownpropertykeys",
            domain: "Mutable parsed objects with duplicate, numeric and Unicode keys; undefined versus absent properties, native/typed aliases and nested undefined serialization; deletion is limited to dictionary properties; string-index enumeration uses BMP scalars",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: dictionary_json(Vec::new()),
                },
                Witness {
                    partition: "growth-and-reinsertion",
                    case: dictionary_json((0..40).collect()),
                },
                Witness {
                    partition: "duplicate-and-negative",
                    case: dictionary_json(vec![-7, 0, -7, 8, 0]),
                },
            ],
            strategy: prop::collection::vec(any::<i16>(), 0..40)
                .prop_map(dictionary_json)
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.typed",
            description: "Checked typed aliases of parsed JSON retain identity, mutations and property order",
            specification: "https://tc39.es/ecma262/#sec-json.parse",
            domain: "Parsed closed records and homogeneous arrays, repeated narrowing, native slot replacement, typed/JSON alias writes, array growth and recursive schemas; values stay within declared types",
            witnesses: vec![
                Witness {
                    partition: "empty-array",
                    case: typed_json(Vec::new(), 0),
                },
                Witness {
                    partition: "nested-aliases",
                    case: typed_json(vec![-3, 0, 8], -7),
                },
            ],
            strategy: (prop::collection::vec(any::<i16>(), 0..12), any::<i16>())
                .prop_map(|(values, n)| typed_json(values, n))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.native",
            description: "Native aggregate serialization, creation order and shared typed/JSON storage",
            specification: "https://tc39.es/ecma262/#sec-json.stringify",
            domain: "Native closed records and dense numeric/boolean arrays, bidirectional alias mutation, array growth, recursive shapes and cyclic serialization; writes respect native field types",
            witnesses: vec![
                Witness {
                    partition: "empty-growth",
                    case: native_json(Vec::new(), 0),
                },
                Witness {
                    partition: "shared-storage",
                    case: native_json(vec![-2, 0, 7, 3], -9),
                },
            ],
            strategy: (prop::collection::vec(any::<i16>(), 0..12), any::<i16>())
                .prop_map(|(values, n)| native_json(values, n))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.json.values",
            description: "JSON variants, exact serialization, property order, aliases and catchable syntax errors",
            specification: "https://tc39.es/ecma262/#sec-json-object",
            domain: "Bounded UTF-8 JSON trees with six variants, duplicate and numeric keys, escaped controls and supplementary Unicode; exact command output; own-property mutation and array aliases",
            witnesses: vec![
                Witness {
                    partition: "scalar",
                    case: json_values("null".into()),
                },
                Witness {
                    partition: "order-and-duplicates",
                    case: json_values(
                        r#"{"b":1,"2":2,"a":3,"b":4,"1":5,"__proto__":{"safe":true}}"#.into(),
                    ),
                },
                Witness {
                    partition: "numeric-boundaries",
                    case: json_values("[-0,1e400,5e-324,9007199254740993]".into()),
                },
                Witness {
                    partition: "nested",
                    case: json_values(
                        r#"[{"empty":{},"array":[],"text":"é🙂\n\u0000"},true,false,null]"#.into(),
                    ),
                },
            ],
            strategy: json_strategy().prop_map(json_values).boxed(),
            check: check_equivalence,
        },
    ];
    for (id, description, specification, operation, asynchronous) in [
        (
            "ecma.uint8array.set",
            "Overlapping byte copies and atomic range rejection",
            "https://tc39.es/ecma262/#sec-%typedarray%.prototype.set",
            SET,
            false,
        ),
        (
            "ecma.uint8array.views",
            "Shared byte views, independent copies, numeric conversion and index boundaries",
            "https://tc39.es/ecma262/#sec-typedarray-objects",
            BYTE_VIEWS,
            false,
        ),
        (
            "web.response.bytes",
            "Constructed response snapshot, body use and repeated consumption",
            "https://fetch.spec.whatwg.org/",
            RESPONSE,
            true,
        ),
        (
            "web.request.bytes",
            "Constructed request snapshot, body use and repeated consumption",
            "https://fetch.spec.whatwg.org/",
            REQUEST,
            true,
        ),
    ] {
        let build = move |bytes| bytes_function(bytes, operation, asynchronous);
        contracts.push(Contract{id,description,specification,domain:"Uint8Array sources of 0..256 bytes; each source evaluated over every IEEE-754 boundary input; byte reads checked explicitly",witnesses:vec![Witness{partition:"empty",case:build(vec![])},Witness{partition:"binary-subview",case:build(vec![0,255,128,1,17])}],strategy:prop::collection::vec(any::<u8>(),0..=256).prop_map(build).boxed(),check:check_equivalence});
    }
    contracts.push(Contract{id:"node.stream.writable_to_web",description:"Queued acknowledged writes, writer locks and closed-writer rejection",specification:"https://nodejs.org/api/stream.html#streamwritabletowebstreamwritable",domain:"stdout/stderr Uint8Array writes and subviews; two queued writes, repeat observation, close, rejected released-writer use and a new stdio wrapper; exact output bytes",witnesses:vec![Witness{partition:"empty",case:writer(vec![])},Witness{partition:"binary-and-close",case:writer(vec![0,255,128,10,13])}],strategy:prop::collection::vec(any::<u8>(),1..=256).prop_map(writer).boxed(),check:check_equivalence});
    let callback_timer = Case::Reject {
        source: "import {setTimeout} from 'node:timers';setTimeout(()=>{},1);".into(),
        diagnostic: "Unsupported capability import".into(),
    };
    contracts.push(Contract {
        id: "node.timers.callbacks.unsupported",
        description: "Explicit compiler rejection of callback timers",
        specification: "https://nodejs.org/api/timers.html",
        domain: "Callback timers are outside the supported source domain",
        witnesses: vec![Witness {
            partition: "callback-timer",
            case: callback_timer.clone(),
        }],
        strategy: Just(callback_timer).boxed(),
        check: check_equivalence,
    });
    contracts.extend(additional_contracts());
    contracts.extend(combinator_contracts());
    contracts.push(Contract {
        id: "ecma.promise.await_ordering",
        description: "Await continuations run in registration order after the current job",
        specification: "https://tc39.es/ecma262/#await",
        domain: "Two consumers of one settled or timer-backed promise; fulfillment or rejection, repeated awaits and awaited finalizers; exact effect trace over IEEE-754 input values",
        witnesses: vec![
            Witness { partition: "already-fulfilled", case: await_ordering(2, false, false) },
            Witness { partition: "already-rejected", case: await_ordering(2, false, true) },
            Witness { partition: "pending-fulfilled", case: await_ordering(2, true, false) },
            Witness { partition: "pending-rejected", case: await_ordering(2, true, true) },
        ],
        strategy: (1u32..=5, any::<bool>(), any::<bool>()).prop_map(|(rounds, pending, rejected)| await_ordering(rounds, pending, rejected)).boxed(),
        check: check_equivalence,
    });
    contracts.push(Contract {
        id: "ecma.error.values",
        description: "Error messages, causes and identity across promise rejection and rethrow",
        specification: "https://tc39.es/ecma262/#sec-error-objects",
        domain: "TypeError with a string message and cause; repeated awaits, all/allSettled/race, rethrow and finally; hidden built-in fields; Unicode messages of 0..32 scalar values",
        witnesses: vec![
            Witness { partition: "empty-message", case: error_case(String::new()) },
            Witness { partition: "nul-and-unicode", case: error_case("failed\0é雪😀".into()) },
        ],
        strategy: prop::collection::vec(any::<char>(), 0..=32)
            .prop_map(|chars| error_case(chars.into_iter().collect())).boxed(),
        check: check_equivalence,
    });
    contracts
}

fn error_case(message: String) -> Case {
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
async function fail(message:string):Promise<number> {{
    throw new TypeError(message, {{cause:message}});
}}
export async function run(x:number):Promise<{{value:number,trace:number[]}}> {{
    const message={message};
    const trace:number[]=[];
    const pending=fail(message);
    try {{await pending;}} catch(original) {{
        if(!(original instanceof TypeError)||!(original instanceof Error))throw 90;
        if(original.name!=='TypeError'||original.message!==message||original.cause!==message)throw 91;
        try {{await Promise.all([pending,1]);}} catch(e) {{if(e!==original)throw 92;trace.push(1);}}
        try {{await Promise.race([pending]);}} catch(e) {{if(e!==original)throw 93;trace.push(2);}}
        const settled=await Promise.allSettled([pending]);
        if(settled[0].status!=='rejected'||settled[0].reason!==original)throw 94;
        try {{try {{throw original;}} finally {{trace.push(3);}}}}
        catch(e) {{if(e!==original)throw 95;trace.push(4);}}
        if(JSON.stringify(original)!=='{{}}'||JSON.stringify({{...original}})!=='{{}}')throw 96;
        const encoded=new TextEncoder().encode('message:'+original.message);
        for(let i=0;i<encoded.length;i++){{const byte=encoded[i];if(byte===undefined)throw 97;trace.push(byte);}}
        return {{value:x,trace}};
    }}
    throw 98;
}}
"#,
            message = serde_json::to_string(&message).unwrap()
        ),
    }
}

fn await_ordering(rounds: u32, pending: bool, rejected: bool) -> Case {
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
import {{setTimeout}} from 'node:timers/promises';
async function ready(): Promise<number> {{ return 0; }}
async function produce(value: number): Promise<number> {{
  {delay}
  {reject}
  return value;
}}
async function consume(promise: Promise<number>, trace: number[], tag: number): Promise<void> {{
  trace.push(tag);
  for(let i=0; i<{rounds}; i++) {{
    try {{ await promise; trace.push(tag+10); }}
    catch(e) {{ if(!(e instanceof Error)||e.message!=="shared") throw e; trace.push(tag+30); }}
    finally {{ await ready(); trace.push(tag+20); }}
  }}
}}
export async function run(x: number): Promise<{{value:number,trace:number[]}}> {{
  const trace: number[] = [];
  const shared=produce(x);
  const first=consume(shared,trace,1);
  const second=consume(shared,trace,2);
  trace.push(0);
  await first;
  trace.push(3);
  await second;
  trace.push(4);
  return {{value:x,trace}};
}}
"#,
            delay = if pending { "await setTimeout(1);" } else { "" },
            reject = if rejected {
                "throw new Error(\"shared\");"
            } else {
                ""
            },
        ),
    }
}

const DIRECTORY: &str = r#"
await mkdir('contract-directory');
try {
  await writeFile('contract-directory/value.bin',input);
  const file=await stat('contract-directory/value.bin');
  const directory=await stat('contract-directory');
  const names=await readdir('contract-directory');
  if(!file.isFile()||file.isDirectory()||file.size!==input.length)throw 90;
  if(!directory.isDirectory()||directory.isFile()||names.length!==1||names[0]!=='value.bin')throw 91;
  trace.push(file.size);trace.push(names.length);
  await unlink('contract-directory/value.bin');
} finally {await rmdir('contract-directory');}
"#;
const DECODE: &str = r#"
const decoder=new TextDecoder('utf-8',{fatal:true});
let text='';
try {
  for(let i=0;i<input.length;i++)text+=decoder.decode(input.subarray(i,i+1),{stream:true});
  text+=decoder.decode();trace.push(1);
} catch {trace.push(2);}
const encoded=new TextEncoder().encode(text);
for(let i=0;i<encoded.length;i++){const byte=encoded[i];if(byte===undefined)throw 90;trace.push(byte);}
"#;
fn encoder(text: String) -> Case {
    bytes_function(
        vec![],
        &format!(
            r#"
const text={text};
const encoder=new TextEncoder();
const bytes=encoder.encode(text);
trace.push(encoder.encoding==='utf-8'?1:0);
trace.push(encoder===new TextEncoder()?1:0);
trace.push(bytes.length);
for(let i=0;i<bytes.length;i++)trace.push(bytes[i]);
const view=bytes.subarray(x,-1);
trace.push(view.length);
for(let i=0;i<view.length;i++)trace.push(view[i]);
if(bytes.length>0)bytes[0]=257;
const fresh=encoder.encode(text);
for(let i=0;i<fresh.length;i++)trace.push(fresh[i]);
trace.push(encoder.encode().length);
"#,
            text = serde_json::to_string(&text).unwrap()
        ),
        false,
    )
}
fn headers(text: String) -> Case {
    bytes_function(
        vec![],
        &format!(
            r#"
const headers=new Headers({{'X-Test':{text}}});
headers.append('x-test','second');
const joined=headers.get('X-Test');if(joined===null)throw 90;
const encoded=new TextEncoder().encode(joined);
for(let i=0;i<encoded.length;i++){{const byte=encoded[i];if(byte===undefined)throw 91;trace.push(byte);}}
headers.set('X-Test',' replaced ');
if(headers.get('x-test')!=='replaced'||!headers.has('X-Test'))throw 92;
headers.delete('x-test');if(headers.has('X-Test')||headers.get('X-Test')!==null)throw 93;
"#,
            text = serde_json::to_string(&text).unwrap()
        ),
        false,
    )
}
fn temporal(epoch: i64) -> Case {
    bytes_function(
        vec![],
        &format!(
            r#"
const instant=Temporal.Instant.fromEpochMilliseconds({epoch});
const date=Temporal.PlainDate.from('2024-02-28').add({{days:1}});
const dateTime=Temporal.PlainDateTime.from('2024-02-28T12:30:00.123456789').add({{days:1}});
trace.push(instant.epochMilliseconds);
const encoded=new TextEncoder().encode(instant.toString()+'|'+date.toString()+'|'+dateTime.toString());
for(let i=0;i<encoded.length;i++){{const byte=encoded[i];if(byte===undefined)throw 90;trace.push(byte);}}
"#
        ),
        false,
    )
}
fn additional_contracts() -> Vec<Contract> {
    let directory = |bytes| {
        let Case::Function { source, .. } = bytes_function(bytes, DIRECTORY, true) else {
            unreachable!()
        };
        Case::Function {
            source: format!(
                "import {{mkdir,writeFile,stat,readdir,unlink,rmdir}} from 'node:fs/promises';\n{source}"
            ),
            asynchronous: true,
        }
    };
    let decoder = |bytes| bytes_function(bytes, DECODE, false);
    let exit = |direct| Case::Command {
        source: if direct {
            "console.log('before-exit');process.exit(1);console.log('unreachable');"
        } else {
            "console.log('before-exit');process.exitCode=1;"
        }
        .into(),
        exit: ExitStatus::Failure,
    };
    vec![
        Contract {
            id: "node.fs.directories",
            description: "Directory creation, listing, metadata and deletion",
            specification: "https://nodejs.org/api/fs.html",
            domain: "One directory and binary file; exact size, one listed entry, cleanup and reuse across the numeric boundary corpus",
            witnesses: vec![
                Witness {
                    partition: "empty-file",
                    case: directory(vec![]),
                },
                Witness {
                    partition: "binary-file",
                    case: directory(vec![0, 255, 128]),
                },
            ],
            strategy: prop::collection::vec(any::<u8>(), 0..=256)
                .prop_map(directory)
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "web.textencoder.values",
            description: "UTF-8 encoder bytes, identity and independent mutable storage",
            specification: "https://encoding.spec.whatwg.org/#interface-textencoder",
            domain: "Valid Unicode scalars including astral values; byte subviews over IEEE-754 boundaries; empty input, fresh encoding after mutation and encoder identity",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: encoder(String::new()),
                },
                Witness {
                    partition: "scalar-widths-and-nul",
                    case: encoder("Aé水🙂\0".into()),
                },
                Witness {
                    partition: "bom",
                    case: encoder("\u{feff}".into()),
                },
            ],
            strategy: prop::collection::vec(any::<char>(), 0..=32)
                .prop_map(|scalars| encoder(scalars.into_iter().collect()))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "web.textdecoder.streaming",
            description: "Fatal UTF-8 decoding across byte boundaries",
            specification: "https://encoding.spec.whatwg.org/",
            domain: "Fatal UTF-8, one-byte chunks, final flush; arbitrary bytes and valid Unicode scalars; stop at first failure. Strict-by-default decoding and retained unread input after streaming errors use separate subset/spec tests",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: decoder(vec![]),
                },
                Witness {
                    partition: "bom-and-split-scalar",
                    case: decoder("\u{feff}é𝄞\0".as_bytes().to_vec()),
                },
                Witness {
                    partition: "invalid-sequence",
                    case: decoder(vec![65, 0xf0, 0x28, 0x8c, 0x28]),
                },
                Witness {
                    partition: "incomplete-sequence",
                    case: decoder(vec![65, 0xf0, 0x9f]),
                },
            ],
            strategy: prop_oneof![
                prop::collection::vec(any::<u8>(), 0..=32),
                prop::collection::vec(any::<char>(), 0..=32)
                    .prop_map(|scalars| scalars.into_iter().collect::<String>().into_bytes()),
            ]
            .prop_map(decoder)
            .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "web.headers.multimap",
            description: "Header name folding, joined duplicates and mutations",
            specification: "https://fetch.spec.whatwg.org/",
            domain: "ASCII values, case-insensitive names, whitespace trimming, append/get/set/has/delete",
            witnesses: vec![
                Witness {
                    partition: "empty-value",
                    case: headers(String::new()),
                },
                Witness {
                    partition: "trim-and-duplicates",
                    case: headers("  first  ".into()),
                },
            ],
            strategy: "[a-zA-Z0-9 ]{0,32}".prop_map(headers).boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "ecma.temporal.iso",
            description: "Typed ISO Instant, PlainDate and PlainDateTime values",
            specification: "https://tc39.es/proposal-temporal/",
            domain: "Integral epoch milliseconds in +/-8640000000000000, ISO formatting and leap-day addition; pinned Node Temporal enabled explicitly",
            witnesses: vec![
                Witness {
                    partition: "negative-millisecond",
                    case: temporal(-1),
                },
                Witness {
                    partition: "epoch",
                    case: temporal(0),
                },
                Witness {
                    partition: "lower-limit",
                    case: temporal(-8640000000000000),
                },
                Witness {
                    partition: "upper-limit",
                    case: temporal(8640000000000000),
                },
            ],
            strategy: (-8640000000000000i64..=8640000000000000)
                .prop_map(temporal)
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "node.process.exit_status",
            description: "Explicit failure status and immediate process exit",
            specification: "https://nodejs.org/api/process.html",
            domain: "Exit status 1 via process.exitCode or process.exit; exact preceding output and unreachable following output",
            witnesses: vec![
                Witness {
                    partition: "exit-code",
                    case: exit(false),
                },
                Witness {
                    partition: "immediate-exit",
                    case: exit(true),
                },
            ],
            strategy: any::<bool>().prop_map(exit).boxed(),
            check: check_equivalence,
        },
    ]
}

fn combinator(value: i16, settled: bool) -> Case {
    let body = if settled {
        format!(
            r#"
const outcomes=await Promise.allSettled([produce(x),fail({value})]);
const first=outcomes[0];const second=outcomes[1];
if(first.status!=='fulfilled'||second.status!=='rejected')throw 90;
trace.push(first.value);trace.push(second.reason);
"#
        )
    } else {
        format!(
            r#"
const first=produce(x);const second=produce({value});
await first;await second;
const winner=Promise.race([first,second]);
trace.push(await winner);trace.push(await winner);
"#
        )
    };
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
async function produce(value:number):Promise<number>{{return value;}}
async function fail(value:number):Promise<number>{{throw value;}}
export async function run(x:number):Promise<{{value:number,trace:number[]}}> {{
  const trace:number[]=[];{body}return {{value:x,trace}};
}}
"#
        ),
    }
}
fn combinator_contracts() -> Vec<Contract> {
    [("ecma.promise.all_settled",true,"Fulfilled and rejected results retain their status and payload"),("ecma.promise.race",false,"Already-settled contenders retain input order and repeated outcomes")].into_iter().map(|(id,settled,description)|Contract{id,description,specification:"https://tc39.es/ecma262/#sec-promise-objects",domain:"Two numeric tasks; one fulfillment and one rejection for allSettled, two already-completed contenders for race; repeated component calls and IEEE-754 inputs",witnesses:vec![Witness{partition:"zero",case:combinator(0,settled)},Witness{partition:"negative",case:combinator(-7,settled)}],strategy:(-32i16..=32).prop_map(move|value|combinator(value,settled)).boxed(),check:check_equivalence}).collect()
}

fn enumeration(number: i16, present: bool, text: String, values: Vec<i16>) -> Case {
    let text = serde_json::to_string(&text).unwrap();
    let values = serde_json::to_string(&values).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
const dictionary:Record<string,number|undefined>={{b:{number},'10':10,'2':2,a:-0}};
if({present})dictionary.missing=undefined;
const keys=Object.keys(dictionary);
const entries:[string,number|undefined][]=Object.entries(dictionary);
const values=Object.values(dictionary);
dictionary.b=9;delete dictionary.a;dictionary.a=7;
entries[2][1]=12;
if(values[2]!=={number} || dictionary.b!==9 || 1/values[3]!!==-Infinity)throw 1;
const native:{{first:{{count:number}};second?:{{count:number}}}}={{first:{{count:{number}}}}};
if({present})native.second=undefined;
const nativeEntries=Object.entries(native);
const first=native.first;
first.count=first.count+1;
if(nativeEntries[0][1]!==first)throw 2;
native.first={{count:99}};
const parsed=JSON.parse('{{"2":2,"b":{{"count":1}},"a":null}}');
parsed.missing=undefined;
const parsedEntries=Object.entries(parsed);
const parsedValues=Object.values(parsed);
parsed.b.count={number};parsed['2']=8;
if(parsedEntries[0][1]!==2 || parsedEntries[1][1]!==parsed.b || parsedValues[3]!==undefined)throw 3;
const bytes=new Uint8Array({values});
const byteEntries=Object.entries(bytes);
const holders:Record<string,Uint8Array>={{data:bytes}};
const holderEntries=Object.entries(holders);
if(bytes.length>0)holderEntries[0][1][0]=17;
if(holderEntries[0][1]!==bytes)throw 4;
const pair:[string,number]=[{text},{number}];
const tupleValues=Object.values(pair);
pair[1]=pair[1]+1;
const fresh=Object.entries({{present:undefined,count:{number}}});
const record={{n:{number},text:{text},flag:{present}}};
const array:number[]={values};
console.log(JSON.stringify({{keys,entries,values,dictionary,nativeEntries,parsedEntries,parsedValues,byteEntries,bytesFirst:bytes[0],pair,tupleValues,fresh,record:Object.entries(record),array:Object.entries(array),string:Object.entries({text}),empty:[Object.values([]),Object.entries({{}}),Object.entries(false),Object.values(12)]}}));
"#
        ),
    }
}

fn optional_record(number: i16, present: bool, text: String) -> Case {
    let text = serde_json::to_string(&text).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
interface RecordNode {{ value:number; next?:RecordNode }}
interface Item {{ count:number; required:number|undefined; label?:string; maybe?:number; items?:number[]; child?:RecordNode }}
const item:Item={{required:undefined,count:{number}}};
if({present})item.label=undefined;
const snapshot=Object.keys(item);
item.maybe=-0;item.items=[{number},2];item.child={{value:3,next:{{value:4}}}};
const child:RecordNode=item.child!;
child.value={number}+1;
item.label={text};delete item.maybe;delete item.label;item.label={text};
item.maybe=undefined;
const parsed:Item=JSON.parse('{{"count":2,"required":3,"extra":4}}');
parsed.required=undefined;parsed.label={text};delete parsed.label;parsed.label=undefined;
const bytes:{{bytes?:Uint8Array}}={{}};bytes.bytes=new Uint8Array([1,2]);
const byte=bytes.bytes[1];delete bytes.bytes;
const empty:{{n?:number}}={{}};
const before=Object.hasOwn(empty,'n');empty.n=undefined;
console.log(JSON.stringify({{item,snapshot,keys:Object.keys(item),parsed,parsedKeys:Object.keys(parsed),child:child===item.child,byte,bytes:Object.keys(bytes),before,after:Object.hasOwn(empty,'n'),emptyKeys:Object.keys(empty)}}));
"#
        ),
    }
}

fn constrained_dictionary(count: i16, present: bool, entries: Vec<i16>) -> Case {
    let entries = serde_json::to_string(&entries).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
interface Bag {{ [key:string]:number|undefined; count:number; maybe?:number; required:number|undefined }}
type Payload = {{ [key:string]:JsonValue; label:string; count:number; child:{{n:number}} }};
type Tree = {{ [key:string]:Tree|undefined; next?:Tree }};
const bag:Bag={{count:{count},required:undefined}};
if({present})bag.maybe=undefined;
const before=Object.keys(bag);
const alias:Record<string,number|undefined>=bag;
const entries:number[]={entries};
for(let i=0;i<entries.length;i++)alias[i]=entries[i];
const snapshot=Object.values(bag);
alias.count=bag.count+1;
delete bag.maybe;bag.maybe={count};
const narrowed:Bag=alias as Bag;
const parsed:Bag=JSON.parse('{{"count":2,"required":3}}');
parsed.required=undefined;parsed.maybe=4;delete parsed.maybe;
const payload:Payload={{label:'é🙂',count:{count},child:{{n:3}}}};
const wide:Record<string,JsonValue>=payload;
wide.count={count}+2;
const child:{{n:number}}=payload.child;child.n=8;
const leaf:Tree={{}};const tree:Tree={{next:leaf}};tree.extra=leaf;
console.log(JSON.stringify({{bag,before,snapshot,parsed,payload,tree,identity:bag===alias && bag===narrowed,required:Object.hasOwn(bag,'required'),maybe:Object.hasOwn(bag,'maybe')}}));
"#
        ),
    }
}

fn json_replacer(text: String, amount: i16, gap: String) -> Case {
    let text = serde_json::to_string(&text).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
function identity(key:string,value:JsonValue|undefined):JsonValue|undefined {{return value;}}
let calls=0;let outer=0;const trace:string[]=[];
let output:string|undefined=undefined;
try {{
  output=JSON.stringify(JSON.parse({text}),(key,value)=>{{
    trace.push(key);trace.push(typeof value);
    try {{
      if(key==='drop' || key==='0') return undefined;
      if(key==='replace') return {{tag:{amount}}};
      if(typeof value==='number') {{const n:number=value;return n+{amount};}}
      return value;
    }} finally {{calls=calls+1;}}
  }},{gap});
}} finally {{outer=outer+1;}}
const shared={{n:1}};const aliases={{a:shared,b:shared}};
const saved=JSON.stringify(aliases,(key,value)=>{{if(key==='b')shared.n={amount};return value;}});
const array=[1,2];
const grown=JSON.stringify(array,(key,value)=>{{if(key==='0'){{array[1]={amount};array.push(9);}}return value;}});
const dictionary:Record<string,number>={{a:1,b:2,c:3}};let observed='';
const changed=JSON.stringify(dictionary,(key,value)=>{{
  observed=observed+key+':'+typeof value+';';
  if(key==='a'){{dictionary.b={amount};delete dictionary.c;dictionary.d=4;}}
  if(key==='c')return 10;
  return value;
}});
const cycle:Record<string,JsonValue>={{x:1}};cycle.self=cycle;
const filtered=JSON.stringify(cycle,(key,value)=>{{if(key==='self')return undefined;return value;}});
let failed=false;let cycleCalls=0;
try{{JSON.stringify(cycle,(key,value)=>{{cycleCalls=cycleCalls+1;return value;}});}}catch(e){{failed=e instanceof TypeError;}}
let nestedCalls=0;
const nested=JSON.stringify({{x:2}},(key,value)=>{{
  if(key==='x')return JSON.stringify([value],(_inner,next)=>{{nestedCalls=nestedCalls+1;return next;}});
  return value;
}});
const plain=JSON.stringify(JSON.parse({text}),identity,{gap});
const absent=JSON.stringify(null,()=>undefined);
const replaced=JSON.stringify(undefined,()=>{amount});
console.log(JSON.stringify({{output,trace,calls,outer,saved,grown,changed,observed,filtered,failed,cycleCalls,nested,nestedCalls,plain,absent:typeof absent,replaced}}));
"#
        ),
    }
}

fn json_reviver(text: String, amount: i16) -> Case {
    let text = serde_json::to_string(&text).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
function identity(key:string,value:JsonValue|undefined):JsonValue|undefined {{return value;}}
let calls=0;let outer=0;const trace:string[]=[];
let result:JsonValue|undefined=undefined;
try {{
  result=JSON.parse({text},((key:string,value:JsonValue|undefined,context:JsonValue):JsonValue|undefined=>{{
    trace.push(key);trace.push(typeof value);
    const source=(context as {{[key:string]:JsonValue}}).source;
    if(source!==undefined) {{const raw:string=source as string;trace.push(raw);}} else {{trace.push('<none>');}}
    try {{
      if(key==='drop') return undefined;
      if(typeof value==='number') {{const number:number=value;return number+{amount};}}
      return value;
    }} finally {{calls=calls+1;}}
  }}) as unknown as (key:string,value:unknown)=>unknown);
}} finally {{outer=outer+1;}}
if(outer!==1) throw 1;
const plain=JSON.parse({text},identity);
let nestedCount=0;
const nested=JSON.parse('1',(_key,value)=>{{
  return JSON.parse('2',(_inner,next)=>{{nestedCount=nestedCount+1;return next;}});
}});
const missing=JSON.parse('null',()=>undefined);
console.log(JSON.stringify({{result,trace,calls,plain,nested,nestedCount,missing:typeof missing}}));
"#
        ),
    }
}

fn json_options(text: String, keys: Vec<u8>, space: String) -> Case {
    let keys: Vec<&str> = keys
        .into_iter()
        .map(|index| ["b", "a", "2", "1", "child", "text", "keep", "x", "0"][index as usize])
        .collect();
    let keys = serde_json::to_string(&keys).unwrap();
    let text = serde_json::to_string(&text).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
type State = {{trace:number}};
function first(state:State,value:JsonValue):JsonValue {{state.trace=state.trace*10+1;return value;}}
function second(state:State,value:Record<string,number>):string[] {{state.trace=state.trace*10+2;value.a=9;return ['a'];}}
function third(state:State):number {{state.trace=state.trace*10+3;return 2;}}
const input = JSON.parse({text});
const filter:JsonValue[] = {keys};
filter.push(2);filter.push(false);filter.push(null);filter.push({{}});
const space:JsonValue = {space};
const native={{b:[1,2],a:'é🙂'}};
const nativeJson:JsonValue=native;
const view:{{a:string,b:number[]}}=nativeJson as unknown as {{a:string,b:number[]}};
const dictionary:Record<string,number>={{b:-0,a:3}};dictionary[1]=9;
const state={{trace:0}};
const effect=JSON.stringify(first(state,dictionary),second(state,dictionary),third(state));
if(state.trace!==123) throw 1;
const cycle=JSON.parse('{{"keep":1}}');cycle.self=cycle;
const excluded=JSON.stringify(cycle,['keep'],space as unknown as string);
const missing=JSON.stringify(undefined,filter as unknown as (string|number)[],space as unknown as string);
if(typeof missing!=='undefined') throw 2;
console.log(JSON.stringify([
  JSON.stringify(input,filter as unknown as (string|number)[],space as unknown as string), JSON.stringify(input,null,space as unknown as string),
  JSON.stringify(native,filter as unknown as (string|number)[],space as unknown as string), JSON.stringify(view,filter as unknown as (string|number)[],space as unknown as string),
  JSON.stringify(dictionary,filter as unknown as (string|number)[],space as unknown as string), JSON.stringify(input,[],space as unknown as string),
  JSON.stringify(input,false as unknown as null,space as unknown as string), JSON.stringify([undefined,-0,0/0,1/0],filter as unknown as (string|number)[],space as unknown as string),
  effect, excluded, typeof missing
]));
"#
        ),
    }
}

fn typed_dictionary(values: Vec<i16>, n: i16) -> Case {
    let values = serde_json::to_string(&values).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue=null|boolean|number|string|JsonValue[]|{{[key:string]:JsonValue}};
interface Numbers {{[key:string]:number}}
type Tree={{[key:string]:Tree}};
function numbers(value:JsonValue):Numbers {{return value as unknown as Numbers;}}
const input:number[]={values};
const dictionary:Record<string,number>={{last:-0,first:{n}}};
for(let i=0;i<input.length;i++) dictionary[i]=input[i];
const keys=Object.keys(dictionary);
const snapshot=Object.values(dictionary);
for(let i=0;i<input.length;i++) {{delete dictionary[i];dictionary[i]=input[i]+1;}}
const alias:Numbers=dictionary;
alias.first={n}+2;
const packed:JsonValue=dictionary;
packed.extra={n}+3;
if(alias!==dictionary || dictionary.extra!=={n}+3) throw 1;
const strings:Record<string,string>={{text:'é🙂'}};
const flags:Record<string,boolean>={{off:false,on:true}};
const arrays:Record<string,number[]>={{input:input}};
const arrayValues=Object.values(arrays);
arrays.input.push({n});
if(arrayValues[0]!==input) throw 2;
const bytes:Record<string,Uint8Array>={{input:new Uint8Array([0,8,255])}};
const byteValues=Object.values(bytes);
byteValues[0][1]=7;
if(bytes.input[1]!==7 || !Object.hasOwn(bytes,'input')) throw 3;
const optional:Record<string,number|undefined>={{gone:undefined,value:{n}}};
const optionalValues=Object.values(optional);
if(optionalValues[0]!==undefined || !Object.hasOwn(optional,'gone')) throw 4;
const parsed=JSON.parse('{{"first":1}}');
const checked=numbers(parsed);
checked.second={n};
parsed.first={n}+4;
if(checked.first!=={n}+4 || checked!==parsed) throw 5;
const before=Object.keys(checked);
delete checked.first;checked.first={n}+5;
const recursive:Tree={{}};recursive.self=recursive;
if(recursive.self!==recursive) throw 6;
let cycle=false;try{{JSON.stringify(recursive);}}catch(e){{cycle=e instanceof TypeError;}}
if(!cycle) throw 7;
delete recursive.self;
console.log(JSON.stringify([keys,snapshot,dictionary,strings,flags,arrays,optional,optionalValues,before,parsed,recursive]));
console.log(JSON.stringify([dictionary.absent+1,strings.absent+'!',flags.absent===undefined]));
"#
        ),
    }
}

fn dictionary_json(values: Vec<i16>) -> Case {
    let mut keys = vec![
        "".to_owned(),
        "é🙂".to_owned(),
        "__proto__".to_owned(),
        "constructor".to_owned(),
        "00".to_owned(),
        "4294967295".to_owned(),
        "4294967294".to_owned(),
    ];
    keys.extend(values.iter().map(ToString::to_string));
    let keys = serde_json::to_string(&keys).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
type Anchor = {{anchor:number}};
function anchor(value: JsonValue): Anchor {{ return value as unknown as Anchor; }}
const root = JSON.parse('{{"anchor":1,"z":2,"z":3,"__proto__":0}}');
const typed = anchor(root);
const again = anchor(root);
const holder = {{item:typed}};
const packed: JsonValue = holder;
const keys: string[] = {keys};
for(let i=0;i<keys.length;i++) root[keys[i]] = i;
const snapshot = Object.keys(root);
for(let i=0;i<keys.length;i++) {{
  if(!Object.hasOwn(root, keys[i])) throw 1;
  if(keys[i] !== '__proto__' && !delete root[keys[i]]) throw 2;
  root[keys[i]] = i+7;
}}
root.missing = undefined;
root.alsoMissing = root.absent;
root.nested = {{gone:undefined,items:[undefined,null,{{present:4,gone:undefined}}]}};
if(!Object.hasOwn(root,'missing') || Object.hasOwn(root,'absent')) throw 3;
if(packed.item !== root || typed !== again) throw 4;
if(!delete root.anchor || Object.hasOwn(root,'anchor')) throw 5;
typed.anchor = 9;
if(again.anchor !== 9) throw 6;
root[0/0] = 1;
root[-0] = 2;
root.true = 3;
const assigned = (root.assigned = 'text');
if(assigned + '!' !== 'text!') throw 8;
const indexed = (root['indexed'] = true);
if(!indexed) throw 9;
if(!Object.hasOwn(root,0/0) || !Object.hasOwn(root,'true')) throw 7;
console.log(JSON.stringify([snapshot,Object.keys(root),root]));
console.log(JSON.stringify([Object.keys('é水'),Object.keys(3),Object.keys(true)]));
const native = {{later:2,first:1}};
console.log(JSON.stringify(Object.keys(native)));
"#
        ),
    }
}

fn typed_json(values: Vec<i16>, n: i16) -> Case {
    let document = format!(
        r#"{{"values":{},"name":"é🙂","extra":true}}"#,
        serde_json::to_string(&values).unwrap()
    );
    let quoted = serde_json::to_string(&document).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
interface Item {{ name: string; values: number[] }}
function item(value: JsonValue): Item {{ return value as unknown as Item; }}
const parsed = JSON.parse({quoted});
const typed = item(parsed);
const again = item(parsed);
const saved: number[] = typed.values;
if(typed !== again || typed !== parsed || saved !== again.values) throw 1;
saved.push({n});
parsed.values[0] = {n}+1;
typed.values[typed.values.length] = {n}+2;
const replacement = JSON.parse('[7,8]');
const holder = {{name:'native',values:[1,2]}};
const json: JsonValue = holder;
json.values = replacement;
const numbers: number[] = holder.values;
numbers[0] = {n}+3;
if(holder.values !== replacement || replacement[0] !== {n}+3) throw 2;
parsed.values = replacement;
typed.name = 'changed';
if(typed.values !== numbers || saved === numbers || again.name !== 'changed') throw 3;
console.log(JSON.stringify([typed,parsed,holder,saved]));
type Tree = {{ value:number; children:Tree[] }};
function tree(value: JsonValue): Tree {{ return value as unknown as Tree; }}
const recursive = JSON.parse('{{"value":1,"children":[]}}');
recursive.children[0] = recursive;
const cycle = tree(recursive);
if(cycle.children[0] !== cycle) throw 4;
console.log('shared');
"#
        ),
    }
}

fn native_json(values: Vec<i16>, n: i16) -> Case {
    let values = serde_json::to_string(&values).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
const values: number[] = {values};
const child = {{z:{n},a:{n}+1}};
const other = {{a:{n}+2,z:{n}+3}};
const native = {{tail:'é🙂',child:child,values:values,flags:[false,true]}};
const reordered = {{values:values,flags:[true,false],child:other,tail:'end'}};
console.log(JSON.stringify([native,reordered]));
const holder = JSON.parse('{{"value":null}}');
holder.value = native;
const alias = holder.value.child;
native.child.a = native.child.a + 1;
alias.z = {n} - 1;
if(alias !== child || alias.a !== native.child.a) throw 1;
const list = holder.value.values;
values.push({n});
list[0] = {n}+7;
list[list.length] = {n}+8;
values.push({n}+9);
holder.value.flags[0] = true;
holder.value.child = other;
if(native.child !== other || alias === native.child || list !== values) throw 2;
console.log(JSON.stringify(native));
console.log(JSON.stringify(reordered));
type Tree = {{ value:number; children:Tree[] }};
const tree: Tree = {{value:1,children:[]}};
const children: Tree[] = tree.children;
children.push(tree);
try {{ JSON.stringify(tree); throw 3; }} catch(e) {{ if(!(e instanceof TypeError)) throw 4; }}
console.log('cycle');
"#
        ),
    }
}

fn json_values(document: String) -> Case {
    let quoted = serde_json::to_string(&document).unwrap();
    Case::Command {
        exit: ExitStatus::Success,
        source: format!(
            r#"
const value=JSON.parse({quoted});
console.log(JSON.stringify(value));
console.log(JSON.stringify(JSON.parse(JSON.stringify(value))));
const object=JSON.parse('{{"item":{{"value":3}},"list":[0],"0":1,"null":null,"text":"é🙂"}}');
const alias=object.item;
const key='value';
alias[key]=4;
if(alias!==object.item||object.item.value!==4)throw 90;
const list=object.list;
list['0']=8;
list[list.length]=alias;
if(list['0']!==8||list.length!==2)throw 91;
object[0]=7;
if(object['0']!==7||object.null!==null||object.missing!==undefined)throw 92;
console.log(JSON.stringify(object));
console.log(JSON.stringify(object.missing));
console.log(JSON.stringify({{z:alias,a:[null,true,1,'é🙂'],b:alias}}));
type JsonValue = null | boolean | number | string | JsonValue[] | {{[key:string]:JsonValue}};
const literal: JsonValue = {{second:2,first:1}};
console.log(JSON.stringify(literal));
try{{JSON.parse('{{"broken":');throw 93;}}
catch(e){{if(!(e instanceof SyntaxError)||!(e instanceof Error))throw 94;console.log('syntax');}}
"#
        ),
    }
}

fn json_strategy() -> BoxedStrategy<String> {
    use serde_json::Value;
    let text = prop::collection::vec(
        prop::sample::select(vec![
            'a', 'z', '0', 'é', '🙂', '\0', '\n', '"', '\\', '\u{2028}',
        ]),
        0..20,
    )
    .prop_map(|text| text.into_iter().collect::<String>())
    .boxed();
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| Value::from(n as f64 / 8.0)),
        text.clone().prop_map(Value::String)
    ];
    leaf.prop_recursive(4, 64, 6, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((text.clone(), inner), 0..6)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
    .prop_map(|value| value.to_string())
    .boxed()
}
