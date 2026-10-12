use crate::core_program::Program;

#[test]
fn error_classes_causes_rethrows_and_absence_preserve_values() {
    let source = r#"
interface Detail { code:string }
export function run():number {
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

#[test]
fn rejected_promises_keep_implicit_error_identity_across_observations() {
    let source = r#"
async function fail():Promise<number> {const values:number[]=[];return values[0];}
export async function run():Promise<number> {
 const pending=fail();let saved=new Error('placeholder');
 try{await pending;}catch(e){saved=e;}
 try{await pending;}catch(e){return e===saved && e instanceof TypeError ? 0 : 1;}
 return 2;
}
"#;
    let mut program = Program::checked(source, "rejection-identity.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn thrown_primitives_compare_by_value_and_errors_compare_by_identity() {
    let source = r#"
export async function run():Promise<number> {
 let first=new Error('placeholder');
 try{throw 42;}catch(e){first=e;if(e!==42)return 1;}
 try{throw 42;}catch(e){if(e!==first)return 2;}
 try{throw false;}catch(e){if(e!==false)return 3;}
 try{throw 'value';}catch(e){if(e!=='value')return 4;}
 try{throw 9007199254740993n;}catch(e){first=e;if(e!==9007199254740993n || 1n===e)return 7;}
 try{throw 9007199254740993n;}catch(e){if(e!==first)return 8;}
 const a=new Error('same');const b=new Error('same');if(a===b)return 5;
 const pending=Promise.reject(NaN);
 try{await pending;}catch(e){first=e;}
 try{await pending;}catch(e){if(e===first)return 6;}
 return 0;
}
"#;
    let mut program = Program::checked(source, "primitive-errors.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}
