use crate::core_program::Program;
use crate::node_oracle::assert_numbers;

#[test]
fn class_calls_preserve_evaluation_order_and_finite_nesting() -> anyhow::Result<()> {
    assert_numbers(
        r#"
let step=0;
function next() { step+=1; return step; }
class Counter {
    optional?:number;
    uninitialized:number|undefined;
    initialized?:number=9;
    field:number=next();
    arg:number;
    constructor(arg:number=next()) { this.arg=arg; }
    assign(extra?:string) {
        const holder:{x?:string} = {};
        holder.x = extra;
        return extra===undefined && ('x' in holder);
    }
}
function checkOmitted(a?:string, b?:string) {
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
export function constructorOrder() {
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
export function implicitConstructor() {
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
    items:T[];
    extra?:string;
    constructor(value:T, items:T[]=[], extra?:string) { this.value=value; this.items=items; this.extra=extra; }
    count():number {
        let total = 0;
        for (let i = 0; i < this.items.length; i++) {
            const item = this.items[i];
            if (item !== undefined) total += 1;
        }
        return total;
    }
}
export function genericConstruction() {
    step=0;
    const explicit:Box<number>=new Box<number>(next(), [10, 20]);
    if(explicit.value!==1 || explicit.field!==2 || step!==2 || explicit.count()!==2 || explicit.extra!==undefined || !('extra' in explicit))return 1;
    step=0;
    const contextual:Box<number>=new Box(next());
    if(contextual.value!==1 || contextual.field!==2 || step!==2 || contextual.count()!==0)return 2;
    step=0;
    const inferred:number=new Box(next()).value;
    if(inferred!==1 || step!==2)return 3;
    const empty:Box<string[]>=new Box([], [["a"], ["b", "c"]]);
    const typed:Box<string[]>=new Box<string[]>([]);
    if(empty.value.length!==0 || empty.count()!==2 || typed.value.length!==0 || typed.count()!==0)return 4;
    return 0;
}

class Series {
    items:number[];
    factor:number;
    constructor(items:number[], factor:number) { this.items=items; this.factor=factor; }
    scale(x:number):number { return x * this.factor; }
    sum():number {
        let total = 0;
        for (let i = 0; i < this.items.length; i++) {
            total += this.scale(this.items[i]);
        }
        return total;
    }
    swapAndSum(replacement:number[]):number {
        let total = 0;
        for (let i = 0; i < this.items.length; i++) {
            total += this.items[i];
            this.items = replacement;
        }
        return total;
    }
}
let retainedSeries:Series|null = null;
export function loopHoisting() {
    const series = new Series([1, 2, 4], 3);
    retainedSeries = series;
    if (retainedSeries.sum() !== 21) return 1;
    if (retainedSeries.swapAndSum([10, 20, 30, 40]) !== 91) return 2;
    return 0;
}

class Nested {
    count:number;
    constructor(count:number=1) { this.count=count; }
    add(n:number=this.count) { return this.count+n; }
    static identity(n:number) { return n; }
    get value() { return this.count; }
    set value(n:number) { this.count=n; }
}
export function finiteNesting() {
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
            ("loopHoisting", &[], 0.0),
            ("finiteNesting", &[], 6.0),
        ],
    )
}

/// Parameter properties are not erasable TypeScript, so Node runs no oracle for them.
#[test]
fn parameter_properties_declare_and_assign_fields() {
    let mut program = Program::new(
        r#"
class Holder<T> {
    constructor(public item: T, readonly count: number = 2) {}
    unwrap() { return this.item; }
}
export function run(x: number): number {
    const holder = new Holder<number>(x);
    return holder.unwrap() * holder.count + Object.keys(holder).length;
}
"#,
    );
    assert_eq!(program.number("run", 20.0), 42.0);
}
