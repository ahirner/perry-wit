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
    let mut contracts = vec![Contract {
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
    }];
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
    for (id, source, diagnostic) in [
        (
            "ecma.json.reviver",
            "JSON.parse('1',(_key,value)=>value);",
            "JSON.parse revivers",
        ),
        (
            "ecma.json.replacer",
            "JSON.stringify({x:1},['x']);",
            "JSON.stringify replacers and formatting",
        ),
        (
            "ecma.json.stringify_options",
            "JSON.stringify({x:1},null,2);",
            "JSON.stringify replacers and formatting",
        ),
    ] {
        let case = Case::Reject {
            source: source.into(),
            diagnostic: diagnostic.into(),
        };
        contracts.push(Contract {
            id,
            description: "Excluded JSON controls are diagnosed at compile time",
            specification: "https://tc39.es/ecma262/#sec-json-object",
            domain: "Compact callback-free JSON profile",
            witnesses: vec![Witness {
                partition: "excluded-control",
                case: case.clone(),
            }],
            strategy: Just(case).boxed(),
            check: check_equivalence,
        });
    }
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
fn filesystem_transfers(bytes: Vec<u8>, text: String) -> Case {
    let operation = format!(
        r#"
const text={text};
const binary=writeFile('transfer.bin',input.subarray(1,-1));
const textual=writeFile('transfer.txt',text,{{encoding:'utf8',flag:'w'}});
await binary;await textual;
try {{
  const pending=readFile('transfer.bin');
  const first=await pending;
  const repeated=await pending;
  const second:Uint8Array=await readFile('transfer.bin',{{encoding:null,flag:'r'}});
  trace.push(first===repeated?1:0);trace.push(first.buffer===second.buffer?1:0);
  for(let i=0;i<first.length;i++)trace.push(first[i]);
  if(first.length>0)first[0]=first[0]+1;
  for(let i=0;i<second.length;i++)trace.push(second[i]);
  const restored=await readFile('transfer.txt',{{encoding:'utf8',flag:'r'}});
  trace.push(restored===text?1:0);trace.push(restored.length);
  const encoded=new TextEncoder().encode(restored);
  for(let i=0;i<encoded.length;i++)trace.push(encoded[i]);
  await writeFile('transfer.bin',input.subarray(0,0));
  trace.push((await readFile('transfer.bin')).length);
  const missing=readFile('missing-transfer-file','utf8');
  try{{await missing;trace.push(0);}}catch{{trace.push(1);}}
  try{{await missing;trace.push(0);}}catch{{trace.push(1);}}
}} finally {{await unlink('transfer.bin');await unlink('transfer.txt');}}
"#,
        text = serde_json::to_string(&text).unwrap()
    );
    let Case::Function { source, .. } = bytes_function(bytes, &operation, true) else {
        unreachable!()
    };
    Case::Function {
        source: format!("import {{readFile,writeFile,unlink}} from 'node:fs/promises';\n{source}"),
        asynchronous: true,
    }
}
fn filesystem_dynamic(bytes: Vec<u8>, text: String) -> Case {
    let operation = format!(
        r#"
const text={text};
await writeFile('dynamic.bin',input);await writeFile('dynamic.txt',text);
try {{
  for(let i=0;i<2;i++) {{
    const pending=load(i===0?'dynamic.txt':'dynamic.bin',i===0?'utf8':undefined);
    const value=await pending;
    const values:Data[]=[value];const record:{{value:Data}}={{value}};
    await load('dynamic.bin',undefined);
    const repeated=await pending;
    trace.push(value===repeated?1:0);trace.push(value===values[0]?1:0);
    trace.push(value===record.value?1:0);
    trace.push(value.length);trace.push(value?1:0);
    if(typeof value==='string') {{trace.push(value===text?1:0);trace.push(value.slice(1).length);}}
    else {{if(value.length>0)value[0]=17;for(let j=0;j<value.length;j++)trace.push(value[j]);}}
    await writeFile('dynamic-copy.bin',value);
    const copy=await load('dynamic-copy.bin',i===0?'utf8':undefined);
    trace.push(copy.length);
    if(typeof copy==='object')for(let j=0;j<copy.length;j++)trace.push(copy[j]);
    else trace.push(copy===text?1:0);
  }}
}} finally {{await unlink('dynamic.bin');await unlink('dynamic.txt');await unlink('dynamic-copy.bin');}}
"#,
        text = serde_json::to_string(&text).unwrap()
    );
    let Case::Function { source, .. } = bytes_function(bytes, &operation, true) else {
        unreachable!()
    };
    Case::Function {
        asynchronous: true,
        source: format!(
            "import {{readFile,writeFile,unlink}} from 'node:fs/promises';\ntype Data=string|Uint8Array;\nasync function load(path:string,encoding:string|undefined):Promise<Data>{{return await readFile(path,{{encoding,flag:'r'}});}}\n{source}"
        ),
    }
}

fn filesystem_metadata(depth: u8) -> Case {
    let mut operation = String::from(
        "const file=await stat('fixture.txt'); trace.push(file.size); trace.push(file.isFile()?1:0); trace.push(file.isDirectory()?1:0);\n",
    );
    let mut path = String::new();
    let mut paths = Vec::new();
    for _ in 0..depth {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str("contract-metadata");
        operation.push_str(&format!("await mkdir('{path}',{{recursive:false}});\n"));
        paths.push(path.clone());
    }
    operation.push_str(&format!("const directory=await stat('./{path}/.');trace.push(directory.isDirectory()?1:0);trace.push(directory.isFile()?1:0);\n"));
    for path in paths.iter().rev() {
        operation.push_str(&format!("await rmdir('{path}');\n"));
    }
    operation.push_str(&format!(
        "try{{await stat('{path}');trace.push(0);}}catch{{trace.push(1);}}\n"
    ));
    let Case::Function { source, .. } = bytes_function(vec![], &operation, true) else {
        unreachable!()
    };
    Case::Function {
        source: format!("import {{stat,mkdir,rmdir}} from 'node:fs/promises';\n{source}"),
        asynchronous: true,
    }
}
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
            id: "node.fs.metadata_lifecycle",
            description: "Native file metadata and nested directory lifecycle",
            specification: "https://nodejs.org/api/fs.html",
            domain: "Existing fixture size and kind; one to six nested directories, normalized paths, removal and missing-path rejection; reuse across numeric inputs. Numeric WASI errors, confinement and cancellation have separate component tests",
            witnesses: vec![
                Witness {
                    partition: "one-directory",
                    case: filesystem_metadata(1),
                },
                Witness {
                    partition: "nested-directories",
                    case: filesystem_metadata(6),
                },
            ],
            strategy: (1u8..=6).prop_map(filesystem_metadata).boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "node.fs.transfers",
            description: "Promise file transfers preserve bytes, UTF-8 text and independent reads",
            specification: "https://nodejs.org/api/fs.html#promises-api",
            domain: "0..256 binary bytes, subview writes, 0..32 BMP scalars including BOM and NUL, empty truncation, repeated promise observation and rejected-read recovery; numeric inputs reuse the instance. Fixed programs seed P6.5; cancellation and malformed UTF-8 use separate component oracles",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: filesystem_transfers(vec![], String::new()),
                },
                Witness {
                    partition: "binary-bom-nul",
                    case: filesystem_transfers(
                        vec![19, 0, 255, 128, 42, 20],
                        "\u{feff}é水\0".into(),
                    ),
                },
            ],
            strategy: (
                prop::collection::vec(any::<u8>(), 0..=256),
                prop::collection::vec(prop::char::range('\0', '\u{ffff}'), 0..=32),
            )
                .prop_map(|(bytes, chars)| filesystem_transfers(bytes, chars.into_iter().collect()))
                .boxed(),
            check: check_equivalence,
        },
        Contract {
            id: "node.fs.dynamic_values",
            description: "Dynamic promise reads retain closed string/byte values and native aliases",
            specification: "https://nodejs.org/api/fs.html#promises-api",
            domain: "0..256 bytes and 0..32 BMP scalars, runtime UTF-8/default encoding, repeated awaits, guarded mutation, array/record aliases across suspension and union writes; malformed UTF-8, binary encoding and numeric errors use separate component assertions. Fixed-program seeds for P6.5",
            witnesses: vec![
                Witness {
                    partition: "empty",
                    case: filesystem_dynamic(vec![], String::new()),
                },
                Witness {
                    partition: "binary-bom-nul",
                    case: filesystem_dynamic(vec![0, 255, 128, 42], "\u{feff}é水\0".into()),
                },
            ],
            strategy: (
                prop::collection::vec(any::<u8>(), 0..=256),
                prop::collection::vec(prop::char::range('\0', '\u{ffff}'), 0..=32),
            )
                .prop_map(|(bytes, chars)| filesystem_dynamic(bytes, chars.into_iter().collect()))
                .boxed(),
            check: check_equivalence,
        },
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
