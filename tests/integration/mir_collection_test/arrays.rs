use crate::core_program::{Counter, Program};
use crate::node_oracle::assert_numbers;

#[test]
fn dense_array_fast_paths_are_independent_of_unrelated_holes() {
    let dense = r#"
export function run(n:number) {
 const values:number[]=[1,2,3,4];
 let sum=0;
 for(let repeat=0;repeat<n;repeat++) {
   for(let i=0;i<values.length;i++){values[i]+=1;sum+=values[i];}
 }
 return sum;
}
export function copies(n:number) {
 const values:number[]=[1,2,3,4];
 let sum=0;
 for(let i=0;i<n;i++) {
   const spread:number[]=[...values];
   const from:number[]=Array.from(values);
   const flat=values.flat(0);
   spread[0]+=1;
   sum+=spread[0]+from[1]+flat[2];
 }
 return sum;
}
"#;
    let mixed = dense.replace(
        "return sum;",
        "const sparse=[,3];return sum+sparse.length-2;",
    );
    let separate = format!("{dense}\nexport function sparse() {{return [,3].join(',');}}");
    let measure = |source: &str, export: &str| {
        let mut consumed = Vec::new();
        for n in [250.0f64, 500.0] {
            let mut program = Program::checked(source, "dense-mixed.ts");
            let before = program.counter(Counter::Allocations);
            let (result, fuel) = program.call(export, &[wasmtime::Val::F64(n.to_bits())]);
            let expected = if export == "run" {
                2.0 * n * n + 12.0 * n
            } else {
                7.0 * n
            };
            assert_eq!(result[0].unwrap_f64(), expected);
            consumed.push((fuel, program.counter(Counter::Allocations) - before));
        }
        (consumed[1].0 - consumed[0].0, consumed[1].1 - consumed[0].1)
    };
    let baseline = measure(dense, "run").0;
    assert_eq!(measure(&mixed, "run").0, baseline);
    assert_eq!(measure(&separate, "run").0, baseline);
    let allocations = measure(dense, "copies").1;
    assert_eq!(measure(&mixed, "copies").1, allocations);
    assert_eq!(measure(&separate, "copies").1, allocations);
}

#[test]
fn concat_and_spread_match_node_for_order_nullability_and_shallow_identity() -> anyhow::Result<()> {
    let source = r#"
interface Cell { value:number }
interface Holder { items:Cell[] }
function mutate(items:Cell[]) { items[0].value=9;return 3; }
function mutateNumbers(a:number[]) { a[0]=9;return [3]; }
let order="";
function take(a:number, ...words:string[]) { return a + ":" + words.join(","); }
function select() { order += "C"; return take; }
function argumentsTuple():[number,string,string] { order += "A"; return [1,"b","c"]; }
function capture(n:number) { return (a:number,b:string) => n+a+b.length; }
function choose(flag:boolean) { return flag ? (a:number,b:string)=>a+b.length : (a:number,b:string)=>a-b.length; }
function surplus() { order += "S"; return 8; }
export function run() {
 const captured = capture(10);
 const pair:[number,string]=[1,"xx"];
 const member={n:5,call:function(a:number,b:string){return this.n+a+b.length;}};
 if(member.call(...pair)!==8 || choose(false)(...pair)!==-1 || choose(true)(...pair)!==3)return 14;
 if(select()(...argumentsTuple())!=="1:b,c" || captured(...pair,surplus())!==13 || order!=="CAS")return 13;
 const a:number[]=[1,2];
 const alias=a;
 const gaps:(number|undefined)[]=[, ...a, , 4, ,];
 if(gaps.length!==6 || 0 in gaps || 3 in gaps || 5 in gaps || gaps[4]!==4 || a.length!==2)return 15;
 const spread=[...a,...mutateNumbers(a),10];
 if(spread[0]!==1 || spread[2]!==3 || alias[0]!==9)return 9;
 const combined=a.concat(mutateNumbers(a),4,...[5,6]);
 if(combined===a || combined[0]!==9 || combined[2]!==3 || combined[5]!==6)return 10;
 combined[0]=7;
 const copiedNumbers=a.concat();
 copiedNumbers[0]=8;
 if(a[0]!==9 || spread.length+combined.length+copiedNumbers.length!==12)return 12;
 const cell:Cell={value:1};const values:Cell[]=[cell];
 const copied=[...values];copied[0].value=2;
 if(values[0]!==cell || cell.value!==2)return 1;
 const nested:number[][]=[[1],[2]];
 const flat=nested.concat([[3]]);
 if(flat.length!==3 || flat[0]!==nested[0])return 2;
 const text:string[]=['é','🙂'];
 const emptyMix=[...[],...text];const emptyCopy=values.concat(...[]);
 if(emptyMix.length!==2 || emptyCopy===values || ''.concat(...[])!=='')return 11;
 const nullable=[null,...text,undefined];
 if(nullable[0]!==null || nullable[1]!=='é' || nullable[3]!==undefined)return 3;
 const optional:(number|null)[]=[1,null];
 const joined=optional.concat(null,[2]);
 if(joined.length!==4 || joined[2]!==null)return 4;
 const repeated=values.concat(values);
 repeated[0].value=7;
 if(repeated[1]!==cell || values[0].value!==7)return 5;
 const empty:number[]=[];empty.push(...empty);
 const self:number[]=[1,2];self.push(...self);
 if(self.length!==4 || self[2]!==1 || self[3]!==2)return 6;
 const words='start'.concat(...text,mutate(values));
 if(words!=='starté🙂3' || cell.value!==9)return 7;
 const holder:Holder={items:values};
 const escape=holder.items.concat([{value:4}]);escape[0]={value:8};
 if(holder.items[0]!==cell || holder.items.length!==1)return 8;
 return 0;
}
"#;
    assert_numbers(source, &[("run", &[], 0.0)])
}

#[test]
fn owned_left_right_and_both_arrays_reuse_capacity_across_joins() {
    let source = r#"
interface Cell { value: number }
export function left(n:number){const borrowed:number[]=[2,3];let total=0;for(let i=0;i<n;i++){const owned:number[]=[1];const joined=owned.concat(borrowed);total+=joined.length+joined[0];}return total;}
export function right(n:number){const borrowed:number[]=[2,3];let total=0;for(let i=0;i<n;i++){const owned:number[]=[1];const joined=borrowed.concat(owned);total+=joined.length+joined[0];}return total;}
export function literal(n:number){const borrowed:number[]=[2,3];let total=0;for(let i=0;i<n;i++){const owned:number[]=[1];const joined=owned.concat([2,3]);total+=joined.length+joined[0];}return total + borrowed[0] - 2;}
export function loopJoin(n: number) {
  let numbers: number[] = [];
  let cells: Cell[] = [];
  for (let i = 0; i < n; i++) {
    if (i % 2 === 0) numbers = [i].concat(numbers);
    else numbers = numbers.concat([i]);
    cells = [...cells, { value: i }];
  }
  return numbers.length + numbers[0] + cells.length + cells[0].value + cells[cells.length - 1].value;
}
"#;
    let mut program = Program::checked(source, "concat-capacity.ts");
    program.stress_collection(true);
    for (export, expected, limit) in [
        ("left", 4000.0, 2100),
        ("right", 5000.0, 2100),
        ("literal", 4000.0, 4100),
    ] {
        let before = program.counter(Counter::Allocations);
        assert_eq!(program.number(export, 1000.0), expected);
        let allocations = program.counter(Counter::Allocations) - before;
        assert!(allocations < limit, "{export}: {allocations} allocations");
    }
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.throwing("loopJoin", 1000.0), (3997.0, 0));
    assert!(program.counter(Counter::Allocations) - before < 5400);
}

#[test]
fn count_only_iteration_uses_constant_fuel() {
    let mut program = Program::new(
        r#"
let text = "";
let values: number[] = [];
export function prepare(n: number) {
  text = "🌱a".repeat(n);
  values = [];
  for (let i=0; i<n; i++) values.push(i);
}
export function count_text() {
  let count = 0;
  for (const _ of text) count++;
  return count;
}
export function count_items() {
  let count = 0;
  for (const _ of values) count++;
  return count;
}
export function text_length() { return text.length; }
export function item_length() { return values.length; }
let visits = 0;
export function observed() {
  visits = 0;
  for (const _ of values) visits++;
  return visits;
}
export function until_three() {
  let count = 0;
  for (const n of values) { if (n === 3) break; count++; }
  return count;
}
"#,
    );
    let mut fuel = Vec::new();
    for n in [0_u32, 1, 4096] {
        program.call("prepare", &[wasmtime::Val::F64(f64::from(n).to_bits())]);
        let costs = [
            ("count_text", "text_length", 2 * n),
            ("count_items", "item_length", n),
        ]
        .map(|(name, length, expected)| {
            let (result, cost) = program.call(name, &[]);
            assert_eq!(result[0].unwrap_f64(), f64::from(expected));
            let (_, baseline) = program.call(length, &[]);
            cost as i64 - baseline as i64
        });
        assert_eq!(
            program.call("until_three", &[]).0[0].unwrap_f64(),
            f64::from(n.min(3))
        );
        assert_eq!(
            program.call("observed", &[]).0[0].unwrap_f64(),
            f64::from(n)
        );
        fuel.push(costs);
    }
    assert!(fuel.windows(2).all(|pair| pair[0] == pair[1]), "{fuel:?}");
}

/// Inline and named callbacks see `(item, index, array)` over the original length, propagate
/// exceptions, run `finally`, return through `switch` and endless loops (an empty return is
/// `undefined`), and `for...of` visits elements pushed during iteration and string scalars;
/// all under forced collection with poisoning.
#[test]
fn array_callbacks_and_iteration_survive_forced_collection() {
    let source = r#"
function twice(n:number) {return n*2;}
function nothing(n:number) {}
function missingDefault():number {throw new Error('missing');}
function refill(values:number[]) {values.push(1,2);return 8;}
export function callbacks() {
 let calls=0;
 const numbers:number[]=[1,2,3];
 const mapped=numbers.map((n,i,a)=>{calls++;if(i===0)a.push(4);return n+i;});
 const doubled=mapped.filter(n=>n>1).map(twice);
 if(calls!==3 || mapped.length!==3 || doubled[0]!==6 || doubled[1]!==10)return 1;
 const values:number[]=[1,2];
 let trace=0;
 try {values.map((n,i,a)=>{trace=trace*10+n;if(i===1)throw new Error('stop');a[1]=4;return n;});return 2;}
 catch(e){if(e.message!=='stop')return 3;}
 const finished=values.map(n=>{try {return n*2;}finally {trace++;}});
 if(trace!==16 || finished[1]!==8)return 4;
 const wrapped=values.map(n=>n+(trace as number));
 if(wrapped[0]!==17 || wrapped[1]!==20)return 7;
 const small:number[]=[1,2,3];
 const cases:number[]=small.map(n=>{switch(n){case 1:return 7;case 2:default:return n;}});
 const loops:number[]=small.map(n=>{while(true){return n*2;}});
 const empty=small.map(n=>{switch(n){case 1:return;default:return n;}});
 const overridden=small.map(n=>{try{return;}finally{return n;}});
 const ignored=small.map(n=>{return n;return;});
 if(cases[0]+cases[1]+cases[2]+loops[2]!==18 || empty[0]!==undefined || empty[1]!==2 || overridden[0]!==1 || ignored[0]!==1)return 5;
 const optional=small.map(n=>{if(n===1)return 'yes';});
 const bare=small.map(n=>{if(n===1)return;return 'value';});
 if(optional[0]!=='yes' || optional[1]!==undefined || bare[0]!==undefined || bare[1]!=='value')return 6;
 const nested=numbers.map(n=>{const adjust=(v:number)=>v+n;const inner=small.map(adjust);return inner[0]+adjust(1);});
 if(nested[0]!==4 || nested[3]!==10 || small[0]!==1)return 8;
 for(let i=0;i<optional.length;i++)optional[i]=i===1?undefined:'new';
 if(optional[0]!=='new' || optional[1]!==undefined || optional[2]!=='new')return 9;
 const retained=[{label:'old'},{label:'old'}];
 for(let i=0;i<retained.length;i++)retained[i]={label:'cell'+i};
 if(retained[0].label!=='cell0' || retained[1].label!=='cell1')return 10;
 return small.filter(nothing).length;
}
class IterationName { $$perryIteration(): number { return 13; } }
export function iteration() {
 if (new IterationName().$$perryIteration() !== 13) return 6;
 let steps = 0;
 outer: for (const row of [[1, 2], [3]]) {
   for (const n of row) { steps += n; continue outer; }
 }
 if (steps !== 4) return 7;
 const select = (pair: [number, boolean, string]) => {
   const [first, , last] = pair;
   return first + last.length;
 };
 const left: [number, boolean, string][] = [[2, true, 'abc']];
 const right: [number, boolean, string][] = [[6, false, 'x']];
 if (left.map(select)[0] !== 5 || right.map(select)[0] !== 7) return 8;
 const values:number[]=[1,2];
 let visits=0;
 for(const n of values){visits++;if(n===1){values.push(3);continue;}if(n===3)break;}
 let text='';
 for(const word of 'c,d'.split(',')){text+=word;}
 const [first, , missing = visits + 4] = values;
 const [label, count] = ['ok', 3] as [string, number];
 if(first!==1 || missing!==3 || label!=='ok' || count!==3)return 2;
 const [, absent = 7] = [5];
 if(absent!==7)return 3;
 let thrown=0;
 try {const [value = missingDefault()] = []; } catch(e) {thrown++;}
 if(thrown!==1)return 4;
 const exhausted:number[]=[];
 const [filled=refill(exhausted), stillMissing=9]=exhausted;
 if(filled!==8 || stillMissing!==9)return 5;
 for(const scalar of 'é🙂a'){text+=scalar+':';}
 return visits===3 && text==='cdé:🙂:a:' ? 0 : 1;
}
"#;
    let mut program = Program::checked(source, "array-callbacks.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("callbacks", &[]), (0.0, 0));
    assert_eq!(program.call("iteration", &[]).0[0].unwrap_f64(), 0.0);
}
