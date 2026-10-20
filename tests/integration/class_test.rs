use crate::node_oracle::assert_numbers;

#[test]
fn class_calls_preserve_evaluation_order_and_finite_nesting() -> anyhow::Result<()> {
    assert_numbers(
        r#"
let step=0;
function next():number { step+=1; return step; }
class Counter {
    optional?:number;
    uninitialized:number|undefined;
    initialized?:number=9;
    field:number=next();
    arg:number;
    constructor(arg:number=next()) { this.arg=arg; }
    assign(extra?:string):boolean {
        const holder:{x?:string} = {};
        holder.x = extra;
        return extra===undefined && ('x' in holder);
    }
}
function checkOmitted(a?:string, b?:string):boolean {
    const short:number|undefined = a && 1;
    const holder:{x?:string; y?:number} = {};
    holder.x = a;
    holder.y = short;
    return a===b && short===undefined && ('x' in holder) && ('y' in holder);
}
class Rest {
    field:number=next();
    first:number;
    last:number;
    constructor(...values:number[]) { this.first=values[0]; this.last=values[1]; }
}
export function constructorOrder():number {
    step=0;
    const explicit=new Counter(next());
    if(explicit.field!==2 || explicit.arg!==1 || step!==2)return 1;
    if(Object.keys(explicit).join(',')!=='optional,uninitialized,initialized,field,arg')return 6;
    if(explicit.optional!==undefined || !explicit.assign())return 7;
    if(explicit.uninitialized!==undefined)return 8;
    if(explicit.initialized!==9)return 9;
    if(!checkOmitted())return 10;
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

function fail():number { throw new Error("argument"); }
class Implicit { field:number=next(); }
export function implicitConstructor():number {
    step=0;
    const counter=new Implicit(next(),next());
    if(counter.field!==3 || step!==3)return 1;
    step=0;
    try { new Implicit(fail()); } catch(error) { return step; }
    return 2;
}

class Box<T> {
    field:number=next();
    value:T;
    extra?:string;
    constructor(value:T, extra?:string) { this.value=value; this.extra=extra; }
}
export function genericConstruction():number {
    step=0;
    const explicit:Box<number>=new Box<number>(next());
    if(explicit.value!==1 || explicit.field!==2 || step!==2 || explicit.extra!==undefined || !('extra' in explicit))return 1;
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

class Nested {
    count:number;
    constructor(count:number=1) { this.count=count; }
    add(n:number=this.count):number { return this.count+n; }
    static identity(n:number):number { return n; }
    get value():number { return this.count; }
    set value(n:number) { this.count=n; }
}
export function finiteNesting():number {
    const counter=new Nested(new Nested().count);
    const another=new Nested(counter.add(counter.add(1)));
    counter.value=another.value;
    return Nested.identity(Nested.identity(counter.add()));
}
"#,
        &[
            ("constructorOrder", &[], 0.0),
            ("implicitConstructor", &[], 0.0),
            ("genericConstruction", &[], 0.0),
            ("finiteNesting", &[], 6.0),
        ],
    )
}
