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
    let mut contracts = Vec::new();
    for (id, description, specification, operation, asynchronous) in [
        (
            "ecma.uint8array.set",
            "Overlapping byte copies and atomic range rejection",
            "https://tc39.es/ecma262/#sec-%typedarray%.prototype.set",
            SET,
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
    contracts
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
            id: "web.textdecoder.streaming",
            description: "Fatal UTF-8 decoding across byte boundaries",
            specification: "https://encoding.spec.whatwg.org/",
            domain: "Fatal UTF-8, one-byte chunks, final flush; compare successful prefix bytes and normalized decoding failure",
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
            strategy: prop::collection::vec(any::<u8>(), 0..=32)
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
