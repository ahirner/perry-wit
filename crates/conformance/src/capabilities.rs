//! Concrete standard API domains. Source generation is pure, and adapters own all host effects.
use crate::{
    execution::check_equivalence,
    program::{FIXTURE, Form, Number, Program, literal},
    registry::{Case, Contract, ExitStatus, Witness},
};
use proptest::prelude::*;

fn bytes_function(bytes: Vec<u8>, operation: &str, asynchronous: bool) -> Case {
    Case::Function {
        asynchronous,
        source: bytes_source(&bytes, operation, asynchronous),
    }
}
/// A `run` export applying `operation` to `input`, a `Uint8Array` of `bytes`.
fn bytes_source(bytes: &[u8], operation: &str, asynchronous: bool) -> String {
    let prefix = if asynchronous { "async " } else { "" };
    let result = if asynchronous {
        "Promise<{value:number,trace:number[]}>"
    } else {
        "{value:number,trace:number[]}"
    };
    format!(
        r#"
export {prefix}function run(x:number):{result} {{
  const input=new Uint8Array({bytes});
  const trace:number[]=[];
  {operation}
  return {{value:x,trace}};
}}
"#,
        bytes = literal(bytes)
    )
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
            bytes = literal(&bytes)
        ),
    }
}
pub(crate) fn contracts() -> Vec<Contract> {
    let mut contracts = Vec::new();
    for (id, operation, asynchronous) in [
        ("ecma.uint8array.set", SET, false),
        ("web.response.bytes", RESPONSE, true),
        ("web.request.bytes", REQUEST, true),
    ] {
        let build = move |bytes| bytes_function(bytes, operation, asynchronous);
        contracts.push(Contract::new(
            id,
            vec![
                Witness {
                    partition: "empty",
                    case: build(vec![]),
                },
                Witness {
                    partition: "binary-subview",
                    case: build(vec![0, 255, 128, 1, 17]),
                },
            ],
            prop::collection::vec(any::<u8>(), 0..=256)
                .prop_map(build)
                .boxed(),
            check_equivalence,
        ));
    }
    contracts.push(Contract::new(
        "node.stream.writable_to_web",
        vec![
            Witness {
                partition: "empty",
                case: writer(vec![]),
            },
            Witness {
                partition: "binary-and-close",
                case: writer(vec![0, 255, 128, 10, 13]),
            },
        ],
        prop::collection::vec(any::<u8>(), 1..=256)
            .prop_map(writer)
            .boxed(),
        check_equivalence,
    ));
    let callback_timer = Case::Reject {
        source: "import {setTimeout} from 'node:timers';setTimeout(()=>{},1);".into(),
        diagnostic: "Unsupported capability import".into(),
    };
    contracts.push(Contract::new(
        "node.timers.callbacks.unsupported",
        vec![Witness {
            partition: "callback-timer",
            case: callback_timer.clone(),
        }],
        Just(callback_timer).boxed(),
        check_equivalence,
    ));
    contracts.extend(additional_contracts());
    contracts.extend(combinator_contracts());
    contracts.push(Contract::new(
        "ecma.error.values",
        vec![
            Witness {
                partition: "empty-message",
                case: error_case(String::new()),
            },
            Witness {
                partition: "nul-and-unicode",
                case: error_case("failed\0é雪😀".into()),
            },
        ],
        prop::collection::vec(any::<char>(), 0..=32)
            .prop_map(|chars| error_case(chars.into_iter().collect()))
            .boxed(),
        check_equivalence,
    ));
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
            message = literal(&message)
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
            text = literal(&text)
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
    let directory = |bytes: Vec<u8>| Case::Function {
        source: format!(
            "import {{mkdir,writeFile,stat,readdir,unlink,rmdir}} from 'node:fs/promises';\n{}",
            bytes_source(&bytes, DIRECTORY, true)
        ),
        asynchronous: true,
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
        Contract::new(
            "node.fs.directories",
            vec![
                Witness {
                    partition: "empty-file",
                    case: directory(vec![]),
                },
                Witness {
                    partition: "binary-file",
                    case: directory(vec![0, 255, 128]),
                },
            ],
            prop::collection::vec(any::<u8>(), 0..=256)
                .prop_map(directory)
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "web.textdecoder.streaming",
            vec![
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
            prop::collection::vec(any::<u8>(), 0..=32)
                .prop_map(decoder)
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "web.headers.multimap",
            vec![
                Witness {
                    partition: "empty-value",
                    case: headers(String::new()),
                },
                Witness {
                    partition: "trim-and-duplicates",
                    case: headers("  first  ".into()),
                },
            ],
            "[a-zA-Z0-9 ]{0,32}".prop_map(headers).boxed(),
            check_equivalence,
        ),
        Contract::new(
            "ecma.temporal.iso",
            vec![
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
            (-8640000000000000i64..=8640000000000000)
                .prop_map(temporal)
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.process.exit_status",
            vec![
                Witness {
                    partition: "exit-code",
                    case: exit(false),
                },
                Witness {
                    partition: "immediate-exit",
                    case: exit(true),
                },
            ],
            any::<bool>().prop_map(exit).boxed(),
            check_equivalence,
        ),
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
    [
        ("ecma.promise.all_settled", true),
        ("ecma.promise.race", false),
    ]
    .into_iter()
    .map(|(id, settled)| {
        Contract::new(
            id,
            vec![
                Witness {
                    partition: "zero",
                    case: combinator(0, settled),
                },
                Witness {
                    partition: "negative",
                    case: combinator(-7, settled),
                },
            ],
            (-32i16..=32)
                .prop_map(move |value| combinator(value, settled))
                .boxed(),
            check_equivalence,
        )
    })
    .collect()
}

fn retained_fs_witness(form: Form) -> Witness {
    Witness {
        partition: "retained-operation",
        case: Case::Program(Program {
            expression: Number::Mark(Box::new(Number::Input)),
            form,
        }),
    }
}

fn fs_read_utf8_case(payload: String) -> Case {
    let fixture = literal(FIXTURE);
    let payload = literal(&payload);
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
import {{readFile,writeFile}} from 'node:fs/promises';
export async function run(x:number):Promise<{{value:number,trace:number[]}}> {{
  const payload={payload};
  const trace:number[]=[];
  await writeFile('dynamic-utf8.txt',payload);
  const first=readFile('dynamic-utf8.txt','utf8');
  const second=readFile('fixture.txt','utf8');
  const restored=await first;
  const fixture=await second;
  if(restored!==payload||fixture!=={fixture})throw 91;
  const encoded=new TextEncoder().encode(restored);
  trace.push(encoded.length);
  for(let i=0;i<encoded.length;i++){{const byte=encoded[i];if(byte===undefined)throw 92;trace.push(byte);}}
  return {{value:x,trace}};
}}
"#
        ),
    }
}

fn fs_write_text_case(first: String, second: String) -> Case {
    let first = literal(&first);
    let second = literal(&second);
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
import {{readFile,writeFile}} from 'node:fs/promises';
export async function run(x:number):Promise<{{value:number,trace:number[]}}> {{
  const first={first};
  const second={second};
  const trace:number[]=[];
  await writeFile('roundtrip.txt',first);
  const r1=await readFile('roundtrip.txt','utf8');
  if(r1!==first)throw 90;
  await writeFile('roundtrip.txt',second);
  const r2=await readFile('roundtrip.txt','utf8');
  if(r2!==second)throw 92;
  const encoded1=new TextEncoder().encode(r1);
  const encoded2=new TextEncoder().encode(r2);
  trace.push(encoded1.length);
  trace.push(encoded2.length);
  for(let i=0;i<encoded1.length;i++){{const byte=encoded1[i];if(byte===undefined)throw 93;trace.push(byte);}}
  for(let i=0;i<encoded2.length;i++){{const byte=encoded2[i];if(byte===undefined)throw 93;trace.push(byte);}}
  return {{value:x,trace}};
}}
"#
        ),
    }
}

fn fs_metadata_case(bytes: Vec<u8>, text: String) -> Case {
    let bytes = literal(&bytes);
    let text = literal(&text);
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
import {{writeFile,stat,readdir,unlink}} from 'node:fs/promises';
export async function run(x:number):Promise<{{value:number,trace:number[]}}> {{
  const input=new Uint8Array({bytes});
  const text={text};
  const textBytes=new TextEncoder().encode(text);
  const trace:number[]=[];
  await writeFile('alpha.bin',input);
  await writeFile('beta.txt',text);
  try {{
    const fInfo=await stat('fixture.txt');
    const aInfo=await stat('alpha.bin');
    const bInfo=await stat('beta.txt');
    const names=await readdir('.');
    if(!fInfo.isFile()||fInfo.isDirectory()||fInfo.size!==6)throw 93;
    if(!aInfo.isFile()||aInfo.isDirectory()||aInfo.size!==input.length)throw 94;
    if(!bInfo.isFile()||bInfo.isDirectory()||bInfo.size!==textBytes.length)throw 95;
    if(names.length!==3)throw 96;
    trace.push(fInfo.size);
    trace.push(aInfo.size);
    trace.push(bInfo.size);
    trace.push(names.length);
  }} finally {{
    await unlink('alpha.bin');
    await unlink('beta.txt');
  }}
  const remaining=await readdir('.');
  if(remaining.length!==1||remaining[0]!=='fixture.txt')throw 97;
  return {{value:x,trace}};
}}
"#
        ),
    }
}

fn fs_read_bytes_case(bytes: Vec<u8>) -> Case {
    let fixture = literal(FIXTURE);
    Case::Function {
        asynchronous: true,
        source: format!(
            "import {{readFile,writeFile}} from 'node:fs/promises';\n{}",
            bytes_source(
                &bytes,
                &format!(
                    r#"
await writeFile('binary.bin',input);
const first:Uint8Array=await readFile('binary.bin');
const b0=first[0];
if(b0!==undefined)first[0]=(b0+1)%256;
const second=await readFile('binary.bin');
const fixtureBytes=await readFile('fixture.txt');
if(second.byteLength!==input.length||fixtureBytes.byteLength!==6||new TextDecoder().decode(fixtureBytes)!=={fixture})throw 94;
for(let i=0;i<second.byteLength;i++){{const byte=second[i];if(byte===undefined||byte!==input[i])throw 95;trace.push(byte);}}
"#
                ),
                true,
            )
        ),
    }
}

fn fs_write_subview_case(bytes: Vec<u8>, raw_start: usize, raw_end: usize) -> Case {
    let len = bytes.len();
    let a = if len == 0 { 0 } else { raw_start % (len + 1) };
    let b = if len == 0 { 0 } else { raw_end % (len + 1) };
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let expected_len = end - start;
    Case::Function {
        asynchronous: true,
        source: format!(
            "import {{readFile,writeFile}} from 'node:fs/promises';\n{}",
            bytes_source(
                &bytes,
                &format!(
                    r#"
const view=input.subarray({start},{end});
const snapshot=new Uint8Array(view);
const pending=writeFile('view.bin',view);
await pending;
if(view.byteLength>0)view[0]=99;
const restored=await readFile('view.bin');
if(restored.byteLength!=={expected_len})throw 95;
for(let i=0;i<restored.byteLength;i++){{const byte=restored[i];if(byte===undefined||byte!==snapshot[i])throw 96;trace.push(byte);}}
await writeFile('view.bin',view.subarray(0,0));
const empty=await readFile('view.bin');
if(empty.byteLength!==0)throw 97;
trace.push(empty.byteLength);
"#
                ),
                true,
            )
        ),
    }
}

fn fs_recovery_case(bytes: Vec<u8>) -> Case {
    let fixture = literal(FIXTURE);
    Case::Function {
        asynchronous: true,
        source: format!(
            "import {{readFile,writeFile,stat}} from 'node:fs/promises';\n{}",
            bytes_source(
                &bytes,
                &format!(
                    r#"
const pendingUtf8=readFile('missing-utf8.txt','utf8');
let failures=0;
try{{await pendingUtf8;}}catch{{failures++;}}
try{{await pendingUtf8;}}catch{{failures++;}}
try{{await readFile('missing-bytes.bin');}}catch{{failures++;}}
try{{await stat('missing-stat.bin');}}catch{{failures++;}}
if(failures!==4)throw 97;
trace.push(failures);
await writeFile('recovered.bin',input);
const restored=await readFile('recovered.bin');
const fixtureText=await readFile('fixture.txt','utf8');
if(restored.byteLength!==input.length||fixtureText!=={fixture})throw 98;
for(let i=0;i<restored.byteLength;i++){{const byte=restored[i];if(byte===undefined||byte!==input[i])throw 99;trace.push(byte);}}
"#
                ),
                true,
            )
        ),
    }
}

pub(crate) fn fs_contracts() -> Vec<Contract> {
    let unicode_text = || {
        prop::collection::vec(any::<char>(), 0..=48)
            .prop_map(|chars| chars.into_iter().collect::<String>())
    };
    vec![
        Contract::new(
            "node.fs.readFile.utf8",
            vec![
                retained_fs_witness(Form::FileRead),
                Witness {
                    partition: "unicode-and-concurrent-read",
                    case: fs_read_utf8_case("é\0漢𝄞😀\nline2".into()),
                },
            ],
            unicode_text().prop_map(fs_read_utf8_case).boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.fs.writeFile.text",
            vec![
                retained_fs_witness(Form::FileRoundTrip),
                Witness {
                    partition: "overwrite-and-truncate",
                    case: fs_write_text_case("initial-long-payload-é\0漢".into(), "short".into()),
                },
            ],
            (unicode_text(), unicode_text())
                .prop_map(|(a, b)| fs_write_text_case(a, b))
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.fs.metadata",
            vec![
                retained_fs_witness(Form::FileMetadata),
                Witness {
                    partition: "multi-file-stat-and-unlink",
                    case: fs_metadata_case(vec![0, 1, 255, 128, 42], "extra\0é".into()),
                },
            ],
            (prop::collection::vec(any::<u8>(), 0..=256), unicode_text())
                .prop_map(|(bytes, text)| fs_metadata_case(bytes, text))
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.fs.readFile.bytes",
            vec![
                retained_fs_witness(Form::FileBytes),
                Witness {
                    partition: "binary-buffer-isolation",
                    case: fs_read_bytes_case(vec![0, 255, 128, 1, 0, 42, 199]),
                },
            ],
            prop::collection::vec(any::<u8>(), 0..=256)
                .prop_map(fs_read_bytes_case)
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.fs.writeFile.subviews",
            vec![
                retained_fs_witness(Form::FileByteRoundTrip),
                Witness {
                    partition: "arbitrary-subview-slice",
                    case: fs_write_subview_case(vec![19, 0, 255, 128, 42, 20, 77], 1, 5),
                },
            ],
            (
                prop::collection::vec(any::<u8>(), 0..=128),
                0usize..=128,
                0usize..=128,
            )
                .prop_map(|(bytes, start, end)| fs_write_subview_case(bytes, start, end))
                .boxed(),
            check_equivalence,
        ),
        Contract::new(
            "node.fs.readFile.recovery",
            vec![
                retained_fs_witness(Form::FileRejectedRead),
                Witness {
                    partition: "multi-op-rejection-recovery",
                    case: fs_recovery_case(vec![7, 0, 254, 128]),
                },
            ],
            prop::collection::vec(any::<u8>(), 0..=128)
                .prop_map(fs_recovery_case)
                .boxed(),
            check_equivalence,
        ),
    ]
}
