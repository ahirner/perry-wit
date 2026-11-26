use crate::core_program::Program;
use crate::node_oracle::assert_numbers;

#[test]
fn constructor_assignment_types_private_fields() -> anyhow::Result<()> {
    assert_numbers(
        "class Counter { #value; constructor(n) { this.#value=n; } next() { this.#value+=1; return this.#value; } }
         export function run() { const counter=new Counter(41); return counter.next()+Object.keys(counter).length; }", &[("run", &[], 42.0)])
}

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
class Pair<T> {
    z:T; a:T;
    constructor(z:T,a:T) { this.z=z; this.a=a; }
    get(key:string) { return this.z; }
}
function firstValue(x) { return x.get("first"); }
class Rest {
    field:number=next();
    first:number;
    last:number;
    constructor(...values:number[]) { this.first=values[0]; this.last=values[1]; }
}
export function constructorOrder() {
    step=0;
    const pair=new Pair(next(),next());
    if(pair.z!==1 || pair.a!==2 || Object.keys(pair).join(',')!=='z,a')return 11;
    const table=new Map<string,number>(); table.set("first", 7);
    if(firstValue(table)!==7 || firstValue(new Pair("s","t"))!=="s")return 12;
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

class Accumulate {
    sum:number;
    constructor(n:number, tick) {
        try {
            this.sum=tick();
            if (n===0) return;
            this.sum+=new Accumulate(n-1, tick).sum;
        } finally { this.sum+=10; }
    }
}
export function recursiveConstructor() {
    let count=0;
    const total=new Accumulate(2, ()=>++count).sum;
    return total+count;
}

class Nested {
    count:number;
    constructor(count:number=1) { this.count=count; }
    add(n:number=this.count) { return this.count+n; }
    static identity(n:number) { return n; }
    descend(n:number):number { return n === 0 ? this.count : 1 + this.descend(n-1); }
    get value() { return this.count; }
    set value(n:number) { this.count=n; }
}
export function finiteNesting() {
    const counter=new Nested(new Nested().count);
    step=0;
    if(counter.descend(3,next())!==4 || step!==1 || new Nested(2).descend(3)!==5) return -1;
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
            ("recursiveConstructor", &[], 39.0),
        ],
    )
}

#[test]
fn inline_cleanup_uses_its_lexical_loop_and_callers_catch() -> anyhow::Result<()> {
    assert_numbers(
        r#"
let trace = 0;
class Worker {
    offset = 2;
    read(n: number): number {
        for (let outer = 0; outer < 1; outer++) {
            try {
                for (let inner = 0; inner < 1; inner++) return 99;
            } finally { trace = trace * 10 + 1; break; }
        }
        if (n === 2) throw new Error("stop");
        return this.offset + n;
    }
}
export function run(n: number): number {
    trace = 0;
    const worker = new Worker();
    try {
        const values = [n].map(v => {
            try { return worker.read(v); } finally { trace = trace * 10 + 2; }
        });
        trace = trace * 10 + values[0];
    } catch (error) {
        if (error.message !== "stop") return -1;
        trace = trace * 10 + 5;
    } finally { trace = trace * 10 + 6; }
    return trace;
}
"#,
        &[("run", &[1.0], 1236.0), ("run", &[2.0], 1256.0)],
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
