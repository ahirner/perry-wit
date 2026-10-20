use crate::core_program::Program;
use crate::node_oracle::NodeOracle;

#[test]
fn constructor_arguments_precede_fields_defaults_and_body() -> anyhow::Result<()> {
    let source = r#"
let step=0;
function next():number { step+=1; return step; }
class Counter {
    optional?:number;
    uninitialized:number|undefined;
    initialized?:number=9;
    field:number=next();
    arg:number;
    constructor(arg:number=next()) { this.arg=arg; }
}
class Rest {
    field:number=next();
    first:number;
    last:number;
    constructor(...values:number[]) { this.first=values[0]; this.last=values[1]; }
}
export function run():number {
    step=0;
    const explicit=new Counter(next());
    if(explicit.field!==2 || explicit.arg!==1 || step!==2)return 1;
    if(Object.keys(explicit).join(',')!=='optional,uninitialized,initialized,field,arg')return 6;
    if(explicit.optional!==undefined)return 7;
    if(explicit.uninitialized!==undefined)return 8;
    if(explicit.initialized!==9)return 9;
    step=0;
    const omitted=new Counter();
    if(omitted.field!==1 || omitted.arg!==2 || step!==2)return 2;
    step=0;
    const missing:number|undefined=undefined;
    const defaulted=new Counter(missing);
    if(defaulted.field!==1 || defaulted.arg!==2 || step!==2)return 3;
    step=0;
    const rest=new Rest(next(),next());
    if(rest.first!==1 || rest.last!==2 || rest.field!==3 || step!==3)return 4;
    step=0;
    const excess=new Counter(next(),next());
    if(excess.arg!==1 || excess.field!==3 || step!==3)return 5;
    return 0;
}
"#;
    let oracle = NodeOracle::typescript(source, "console.log(run());")?;
    assert_eq!(oracle.run(&[])?.trim(), "0");
    assert_eq!(Program::new(source).call("run", &[]).0[0].unwrap_f64(), 0.0);
    Ok(())
}

#[test]
fn implicit_constructor_preserves_argument_effects_and_failures() {
    let source = r#"
let step=0;
function next():number { step+=1; return step; }
function fail():number { throw new Error("argument"); }
class Counter { field:number=next(); }
export function run():number {
    step=0;
    const counter=new Counter(next(),next());
    if(counter.field!==3 || step!==3)return 1;
    step=0;
    try { new Counter(fail()); } catch(error) { return step; }
    return 2;
}
"#;
    assert_eq!(Program::new(source).outcome("run", &[]), (0.0, 0));
}

#[test]
fn generic_constructors_preserve_context_and_single_evaluation() {
    let source = r#"
let step=0;
function next():number { step+=1; return step; }
class Box<T> {
    field:number=next();
    value:T;
    constructor(value:T) { this.value=value; }
}
export function run():number {
    step=0;
    const explicit:Box<number>=new Box<number>(next());
    if(explicit.value!==1 || explicit.field!==2 || step!==2)return 1;
    step=0;
    const contextual:Box<number>=new Box(next());
    if(contextual.value!==1 || contextual.field!==2 || step!==2)return 2;
    step=0;
    const inferred:number=new Box(next()).value;
    if(inferred!==1 || step!==2)return 3;
    const empty:Box<string[]>=new Box([]);
    const typed:Box<string[]>=new Box<string[]>([]);
    if(empty.value.length!==0 || typed.value.length!==0)return 4;
    return 0;
}
"#;
    assert_eq!(Program::new(source).call("run", &[]).0[0].unwrap_f64(), 0.0);
}

#[test]
fn finite_nested_class_calls_and_repeated_construction_remain_supported() {
    let source = r#"
class Counter {
    count:number;
    constructor(count:number=1) { this.count=count; }
    add(n:number=this.count):number { return this.count+n; }
    static identity(n:number):number { return n; }
    get value():number { return this.count; }
    set value(n:number) { this.count=n; }
}
export function run():number {
    const counter=new Counter(new Counter().count);
    const another=new Counter(counter.add(counter.add(1)));
    counter.value=another.value;
    return Counter.identity(Counter.identity(counter.add()));
}
"#;
    assert_eq!(Program::new(source).call("run", &[]).0[0].unwrap_f64(), 6.0);
}
