//! String behavior through the component boundary, including task lifetimes.
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmtime::error::Context as _;
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
function mark(trace:number[],n:number):number {trace.push(n);return n;}
function text(trace:number[]):string {trace.push(1);return 'abcd';}
export function check():number {
  const value='aé🙂水\0z';
  if(value.length!==6 || value.charAt(1)!=='é' || value.at(-1)!=='z' || value[3]!=='水')return 1;
  if(value.charAt(2)!=='🙂' || value.at(2)!=='🙂' || value[2]!=='🙂' || value.codePointAt(2)!==128578 || value.codePointAt(3)!==27700)return 2;
  if(value.at(-8)!==undefined || value.codePointAt(6)!==undefined || value[1.5]!==undefined || value.charAt(Infinity)!=='')return 3;
  if(value.charAt(NaN)!=='a' || value.charAt(-0.5)!=='a' || value.at(-0.5)!=='a')return 4;
  if(value.slice(2,3)!=='🙂' || value.slice(-3)!=='水\0z' || value.substring(3,2)!=='🙂' || value.substring(-5,1)!=='a')return 5;
  if(value.slice(3,3)!=='' || value.substring(3,3)!=='' || value.slice(Infinity)!=='')return 6;
  if(value.indexOf('水',3)!==3 || value.indexOf('🙂',3)!==-1 || value.indexOf('',3)!==3)return 7;
  if('aaa'.lastIndexOf('aa',1)!==1 || 'a🙂b🙂'.lastIndexOf('🙂',4)!==3 || 'a🙂b🙂'.lastIndexOf('🙂',2)!==1)return 8;
  if(!value.startsWith('🙂',2) || value.startsWith('🙂',3) || !value.endsWith('🙂',3) || value.endsWith('🙂',2))return 9;
  if(!value.startsWith('',3) || !value.endsWith('',3) || !value.includes('水',3) || value.includes('🙂',3))return 10;
  if('abc'.endsWith('c',NaN) || !'abc'.endsWith('c',undefined) || 'ababa'.lastIndexOf('ba',NaN)!==3)return 11;
  if('İΟΣ Σ\u0301 ΣA ßﬃ'.toLowerCase()!=='i̇ος σ́ σa ßﬃ' || 'ßﬃé'.toUpperCase()!=='SSFFIÉ')return 12;
  if('\uFEFF\u2028 x \u00A0'.trim()!=='x' || '\u0085x\u0085'.trim()!=='\u0085x\u0085')return 13;
  if(' x '.trimStart()!=='x ' || ' x '.trimEnd()!==' x')return 14;
  if('é🙂'.repeat(2.8)!=='é🙂é🙂' || 'x'.repeat(NaN)!=='' || ''.repeat(1e300)!=='' || 'x'.repeat(-0.9)!=='')return 15;
  if(JSON.stringify('é::🙂::'.split('::'))!=='["é","🙂",""]' || JSON.stringify('é🙂水'.split(''))!=='["é","🙂","水"]')return 16;
  if(JSON.stringify('a,b,c'.split(',',2))!=='["a","b"]' || 'abc'.split(undefined)[0]!=='abc' || 'abc'.split('',0).length!==0)return 17;
  if('a,b'.split(',',4294967297).length!==1 || 'a,b'.split(',',-1).length!==2)return 18;
  let errors=0;
  try{value[2];}catch(e){if(e instanceof TypeError)errors=errors+1;}
  try{'x'.repeat(Infinity);}catch(e){if(e instanceof RangeError)errors=errors+1;}
  try{'x'.repeat(-1);}catch(e){if(e instanceof RangeError)errors=errors+1;}
  try{'xx'.repeat(2147483647);}catch(e){if(e instanceof RangeError)errors=errors+1;}
  if(errors!==3)return 19;
  const trace:number[]=[];
  const result=text(trace).slice(mark(trace,2),mark(trace,3),mark(trace,4));
  if(result!=='c' || JSON.stringify(trace)!=='[1,2,3,4]')return 20;
  const parsed=JSON.parse('{"text":"aé🙂水"}');
  if(parsed.text.toUpperCase()!=='AÉ🙂水' || parsed.text[1]!=='é' || parsed.text['3']!=='水' || parsed.text['01']!==undefined)return 21;
  let source='é🙂';source=source+'x';
  const prefix=source.slice(0,2);const suffix=source.slice(1);
  const extended=suffix+'!';source=source+'?';
  if(prefix!=='é🙂' || suffix!=='🙂x' || extended!=='🙂x!' || source!=='é🙂x?')return 22;
  if('a'.concat('b','c')!=='abc' || !value.isWellFormed() || value.toWellFormed()!==value)return 23;
  const inferred={point:''.codePointAt(0),at:''.at(0),nested:{value:''[0]}};
  if(JSON.stringify(inferred)!=='{"nested":{}}' || !Object.hasOwn(inferred,'point'))return 24;
  let typed=0;
  try{const declared:{point:number}={point:''.codePointAt(0)};}catch(e){if(e instanceof TypeError)typed=typed+1;}
  try{const declared:{at:string}={at:''.at(0)};}catch(e){if(e instanceof TypeError)typed=typed+1;}
  if(typed!==2)return 25;
  return 0;
}
export function round(input:string,index:number):string {
  const result={lower:input.toLowerCase(),upper:input.toUpperCase(),trim:input.trim(),slice:input.slice(index),substring:input.substring(index),index:input.indexOf('é',index),last:input.lastIndexOf('é',index),point:input.codePointAt(index),split:input.split('é',index)};
  return JSON.stringify(result);
}
"#;
#[test]
fn string_methods_preserve_utf8_spans_scalar_indices_and_task_lifetimes() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package=resolve.push_str("strings.wit","package test:strings; world strings { export check: func() -> u32; export round: func(input:string,index:f64) -> string; }")?;
    let world = resolve.select_world(&[package], Some("strings"))?;
    let compiled = compile_for_world(SOURCE, "strings.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let check = instance.get_typed_func::<(), (u32,)>(&mut store, "check")?;
    let round = instance.get_typed_func::<(&str, f64), (String,)>(&mut store, "round")?;
    let inputs = [
        "",
        "é漢\0a",
        "İΟΣ Σ\u{0301} ΣA ßﬃ",
        "\u{feff} a \u{85}",
        "Ééé",
    ];
    let indices = [
        -10.0,
        -1.0,
        -0.5,
        -0.0,
        0.0,
        1.0,
        1.5,
        2.0,
        7.0,
        4294967297.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        f64::from_bits(0x7ff0000000000001),
    ];
    let oracle=std::process::Command::new("node").args(["--input-type=module","-e",r#"const inputs=JSON.parse(process.argv[1]);const indices=[-10,-1,-.5,-0,0,1,1.5,2,7,4294967297,Infinity,-Infinity,NaN,NaN];console.log(JSON.stringify(inputs.flatMap(input=>indices.map(index=>JSON.stringify({lower:input.toLowerCase(),upper:input.toUpperCase(),trim:input.trim(),slice:input.slice(index),substring:input.substring(index),index:input.indexOf('é',index),last:input.lastIndexOf('é',index),point:input.codePointAt(index),split:input.split('é',index)})))));"#,&serde_json::to_string(&inputs)?]).output()?;
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&oracle.stdout)?;
    assert_eq!(check.call(&mut store, ()).context("check export")?.0, 0);
    for ((input, index), expected) in inputs
        .iter()
        .flat_map(|input| indices.iter().map(move |index| (*input, *index)))
        .zip(expected)
    {
        assert_eq!(
            round
                .call(&mut store, (input, index))
                .with_context(|| format!("round input={input:?}, index={index}"))?
                .0,
            expected,
            "input={input:?}, index={index}"
        );
    }
    let large = "éΣA ".repeat(8192);
    let mut peak = 0;
    for index in 0..16 {
        assert_eq!(check.call(&mut store, ()).context("check export")?.0, 0);
        round.call(&mut store, (&large, 2.0))?;
        if index == 2 {
            peak = store.data().0;
        } else if index > 2 {
            assert_eq!(store.data().0, peak, "string storage leaked across tasks");
        }
    }
    Ok(())
}
