use crate::core_program::Program;

#[test]
fn error_classes_causes_rethrows_primitives_and_rejections_preserve_values() {
    let source = r#"
interface Detail { code:string }
async function fail():Promise<number> {const values:number[]=[];return values[0];}
export async function run():Promise<number> {
 const original=new ReferenceError('original');
 const detail:Detail={code:'E_TEST'};
 const error=new SyntaxError('outer',{cause:original});
 if(error.name!=='SyntaxError' || error.message!=='outer' || !(error instanceof Error))return 1;
 if(error.cause!==original || !(error.cause instanceof ReferenceError))return 2;
 try{throw error;}catch(e){if(e!==error || e.cause.name!=='ReferenceError')return 3;}
 const record=new Error('record',{cause:detail});
 detail.code='E_CHANGED';
 if(record.cause!==detail || record.cause!==record.cause || record.cause.code!=='E_CHANGED')return 4;
 const number=new Error('number',{cause:42});
 if(number.cause!==42)return 5;
 try{throw number.cause;}catch(e){if(e!==number.cause)return 9;}
 const children:Detail[]=[detail];const arrayError=new Error('array',{cause:children});
 if(arrayError.cause.length!==1 || arrayError.cause[0]!==detail)return 10;
 try{throw null;}catch(e){if(e!==null)return 6;}
 try{throw undefined;}catch(e){if(e!==undefined)return 7;}
 let first=new Error('placeholder');
 try{throw 42;}catch(e){first=e;if(e!==42)return 11;}
 try{throw 42;}catch(e){if(e!==first)return 12;}
 try{throw false;}catch(e){if(e!==false)return 13;}
 try{throw 'value';}catch(e){if(e!=='value')return 14;}
 try{throw 9007199254740993n;}catch(e){first=e;if(e!==9007199254740993n || 1n===e)return 15;}
 try{throw 9007199254740993n;}catch(e){if(e!==first)return 16;}
 const a=new Error('same');const b=new Error('same');if(a===b)return 17;
 const nanPending=Promise.reject(NaN);
 try{await nanPending;}catch(e){first=e;}
 try{await nanPending;}catch(e){if(e===first)return 18;}
 const pending=fail();let saved=new Error('placeholder');
 try{await pending;}catch(e){saved=e;}
 try{await pending;}catch(e){if(e!==saved || !(e instanceof TypeError))return 19;}
 try{try{throw error;}finally{return 0;}}catch(e){return 8;}
}
"#;
    let mut program = Program::checked(source, "errors.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn json_syntax_errors_preserve_unicode_crlf_locations() {
    let source = r#"
export function run():number {
 try{JSON.parse('{"é":"🙂",bad}');}catch(e){
  if(!(e instanceof SyntaxError) || e.name!=='SyntaxError')return 1;
  if(e.message!=='Invalid JSON at byte 13 (line 1, column 10)')return 2;
 }
 try{JSON.parse('{\r\n"x":bad}');}catch(e){
  if(e.message!=='Invalid JSON at byte 7 (line 2, column 5)')return 3;
 }
 return 0;
}
"#;
    let mut program = Program::checked(source, "json-errors.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift, Debug, PartialEq)]
#[component(record)]
struct Failure {
    name: String,
    message: String,
}

#[test]
fn initialization_failures_use_declared_results_and_remain_stable() -> anyhow::Result<()> {
    let wit = "package test:initialization; world app { record failure {name:string,message:string} export run:func()->result<f64,failure>; }";
    let source = "function setup():number{throw new RangeError('initialization failed');} const state=setup(); export function run():number{return state;}";
    let component = crate::world::component(wit, "app", "initialization.ts", source)?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) = crate::world::instantiate(&engine, &component)?;
    let run = instance.get_typed_func::<(), (Result<f64, Failure>,)>(&mut store, "run")?;
    for _ in 0..3 {
        let (error,) = run.call(&mut store, ())?;
        assert_eq!(
            error,
            Err(Failure {
                name: "RangeError".into(),
                message: "initialization failed".into()
            })
        );
    }
    Ok(())
}
