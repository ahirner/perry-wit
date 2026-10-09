//! Strict UTF-8 codec behavior, aliasing and task reclamation through components.
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{Component, Linker},
};

#[derive(Default)]
struct Peak(usize);
impl ResourceLimiter for Peak {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.0 = self.0.max(desired);
        Ok(true)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

const SOURCE: &str = r#"
type Decoder = TextDecoder;
interface Holder { decoder: Decoder; encoder?: TextEncoder; }
function encoder():TextEncoder {return new globalThis.TextEncoder();}
function read(decoder:Decoder,input:Uint8Array):string {return decoder.decode(input,{stream:true});}
function mark(trace:number[],n:number,value:boolean):boolean {trace.push(n);return value;}
function label(trace:number[]):string {trace.push(1);return ' UTF-8 ';}
function input(trace:number[]):Uint8Array {trace.push(4);return new Uint8Array([65]);}
function omitted(trace:number[]):void {trace.push(7);}
function optional(present:boolean):boolean|undefined {if(present)return true;return undefined;}
export function check():number {
  const enc=encoder();
  const dec:Decoder=new (TextDecoder as typeof TextDecoder)();
  if(enc.encoding!=='utf-8'||dec.encoding!=='utf-8'||!dec.fatal||dec.ignoreBOM)return 1;
  if(enc===encoder()||dec===new TextDecoder())return 2;
  const holder:Holder={decoder:dec,encoder:enc};
  if(holder.decoder!==dec||holder.encoder!==enc)return 3;
  if(typeof dec!=='object'||holder.encoder.encode('A')[0]!==65)return 20;
  const text='Aé水🙂\0';const bytes=enc.encode(text);
  if(bytes.length!==11||dec.decode(bytes)!==text)return 4;
  const saved=dec.decode(bytes.subarray(1,10));
  bytes[1]=66;
  if(saved!=='é水🙂'||text!=='Aé水🙂\0'||enc.encode(text)[1]!==195)return 5;
  if(enc.encode().length!==0||enc.encode(undefined).length!==0||dec.decode()!==''||dec.decode(undefined,null)!=='')return 6;
  const trace:number[]=[];
  const ordered=new TextDecoder(label(trace),{ignoreBOM:mark(trace,2,false),fatal:mark(trace,3,true)});
  if(ordered.decode(input(trace),{ignored:mark(trace,5,true),stream:mark(trace,6,true)})!=='A')return 7;
  if(JSON.stringify(trace)!=='[1,2,3,4,5,6]')return 8;
  const duplicate=new TextDecoder(undefined,{fatal:mark(trace,8,false),fatal:mark(trace,9,true)});
  if(duplicate.decode(new Uint8Array([240,159]),{stream:false,stream:true})!=='')return 9;
  if(duplicate.decode(new Uint8Array([153,130]),{stream:omitted(trace)})!=='🙂')return 10;
  if(duplicate.decode(omitted(trace))!==''||enc.encode(omitted(trace)).length!==0)return 11;
  if(JSON.stringify(trace)!=='[1,2,3,4,5,6,8,9,7,7,7]')return 21;
  const defaults=new TextDecoder(undefined,{fatal:optional(false),ignoreBOM:optional(false)});
  if(!defaults.fatal||defaults.ignoreBOM||defaults.decode(new Uint8Array([239,187,191,65]),{stream:optional(false)})!=='A')return 22;
  if(defaults.decode(new Uint8Array([239,187,191,66]))!=='B')return 23;
  let errors=0;
  try{new TextDecoder('latin1');}catch(e){if(e instanceof RangeError)errors++;}
  try{new TextDecoder('\u00a0utf8');}catch(e){if(e instanceof RangeError)errors++;}
  try{new TextDecoder('utf8',{fatal:false});}catch(e){if(e instanceof TypeError)errors++;}
  try{dec.decode(new Uint8Array([192,128]));}catch(e){if(e instanceof TypeError)errors++;}
  try{dec.decode(new Uint8Array([237,160,128]));}catch(e){if(e instanceof TypeError)errors++;}
  try{dec.decode(new Uint8Array([244,144,128,128]));}catch(e){if(e instanceof TypeError)errors++;}
  try{dec.decode(new Uint8Array([240,159]));}catch(e){if(e instanceof TypeError)errors++;}
  if(errors!==7)return 12;
  if(dec.decode(new Uint8Array([65]))!=='A')return 13;
  const alias=holder.decoder;
  const partial=new Uint8Array([239,187]);
  if(read(alias,partial)!=='')return 14;
  partial[0]=65;
  if(read(dec,new Uint8Array([191,65]))!=='A')return 15;
  if(read(alias,new Uint8Array([239,187,191]))!=='\uFEFF'||dec.decode()!=='')return 16;
  if(dec.decode(new Uint8Array([239,187,191,66]))!=='B')return 17;
  const keep=new TextDecoder('unicode-1-1-utf-8',{ignoreBOM:true});
  if(!keep.ignoreBOM||keep.decode(new Uint8Array([239,187,191]))!=='\uFEFF')return 18;
  const queue=new TextDecoder();let result='';
  try{queue.decode(new Uint8Array([65,224,128,66]),{stream:true});}catch{result+='!';}
  try{queue.decode(new Uint8Array([67]),{stream:true});}catch{result+='!';}
  result+=queue.decode(new Uint8Array([239,187,191,68]),{stream:true});
  result+=queue.decode();result+=queue.decode(new Uint8Array([239,187,191,69]));
  if(result!=='!!BC\uFEFFDE')return 19;
  return 0;
}
export function decodeStream(input:Uint8Array,width:number,ignore:boolean):string {
  const decoder=new TextDecoder('utf-8',{fatal:true,ignoreBOM:ignore});
  let text='';
  try {
    for(let i=0;i<input.length;i+=width)text+=decoder.decode(input.subarray(i,i+width),{stream:true});
    text+=decoder.decode();return 'ok:'+text;
  }catch(e){if(e instanceof TypeError)return 'error:'+text;throw e;}
}
"#;

#[test]
fn codecs_preserve_options_utf8_state_and_independent_storage() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("codecs.wit", "package test:codecs; world codecs { export check: func() -> u32; export decode-stream: func(input:list<u8>,width:u32,ignore:bool) -> string; }")?;
    let world = resolve.select_world(&[package], Some("codecs"))?;
    let compiled = compile_for_world(SOURCE, "codecs.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let check = instance.get_typed_func::<(), (u32,)>(&mut store, "check")?;
    let stream =
        instance.get_typed_func::<(&[u8], u32, bool), (String,)>(&mut store, "decode-stream")?;
    assert_eq!(check.call(&mut store, ())?.0, 0);
    let mut inputs = vec![
        vec![],
        "\u{feff}Aé水🙂\0\u{feff}".as_bytes().to_vec(),
        vec![65, 224, 128, 66],
        vec![65, 240, 159],
        vec![237, 160, 128],
        vec![244, 143, 191, 191],
        vec![244, 144, 128, 128],
    ];
    let mut state = 0x12345678u32;
    for size in 0..64 {
        let mut bytes = Vec::new();
        let mut text = String::new();
        for _ in 0..size {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            bytes.push((state >> 24) as u8);
            if let Some(scalar) = char::from_u32(state % 0x110000) {
                text.push(scalar);
            }
        }
        inputs.push(bytes);
        inputs.push(text.into_bytes());
    }
    let oracle=std::process::Command::new("node").args(["--input-type=module","-e",r#"
const inputs=JSON.parse(process.argv[1]);
console.log(JSON.stringify(inputs.flatMap(input=>[1,2,3,4,7,33].flatMap(width=>[false,true].map(ignoreBOM=>{
 const decoder=new TextDecoder('utf-8',{fatal:true,ignoreBOM});let text='';const bytes=new Uint8Array(input);
 try{for(let i=0;i<bytes.length;i+=width)text+=decoder.decode(bytes.subarray(i,i+width),{stream:true});text+=decoder.decode();return 'ok:'+text;}catch{return 'error:'+text;}
})))));
"#,&serde_json::to_string(&inputs)?]).output()?;
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&oracle.stdout)?;
    let mut expected = expected.into_iter();
    for input in &inputs {
        for width in [1, 2, 3, 4, 7, 33] {
            for ignore in [false, true] {
                assert_eq!(
                    stream.call(&mut store, (input, width, ignore))?.0,
                    expected.next().unwrap(),
                    "input={input:?},width={width},ignore={ignore}"
                );
            }
        }
    }
    let large = "é水🙂".repeat(8192).into_bytes();
    let mut peak = 0;
    for iteration in 0..12 {
        assert_eq!(check.call(&mut store, ())?.0, 0);
        assert_eq!(
            stream.call(&mut store, (&large, 33, false))?.0,
            format!("ok:{}", std::str::from_utf8(&large)?)
        );
        if iteration == 2 {
            peak = store.data().0;
        } else if iteration > 2 {
            assert_eq!(store.data().0, peak, "codec task storage leaked");
        }
    }
    Ok(())
}
