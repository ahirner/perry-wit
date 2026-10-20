//! Observable array, Map, and Set semantics compared against Node.
use crate::node_oracle::assert_numbers;

#[test]
fn collections_preserve_evaluation_order_and_key_semantics() -> anyhow::Result<()> {
    let error = perry_wit::compile_core(
        "export function run():number { return [,3,,1].sort()[0]; }",
        "sparse-sort.ts",
    )
    .expect_err("sparse literals remain outside the dense-array profile");
    assert!(error.to_string().contains("sparse array spreads"));
    assert_numbers(
        r#"
function append(a:number[], value:number, result:number):number {a.push(value);return result;}
function shorten(a:number[]):number {a.pop();return -2;}
function missingEnd(a:number[]):number|undefined {a.push(4);return undefined;}
export function argumentOrder():number {
 const sliced:number[]=[1,2];
 const alias=sliced;
 const copy=sliced.slice(append(alias,3,-2),append(alias,4,4));
 if(copy.join(',')!=='3,4' || sliced.join(',')!=='1,2,3,4' || copy===alias)return 1;
 const shrinking:number[]=[1,2,3];
 if(shrinking.slice(shorten(shrinking)).join(',')!=='1,2')return 2;
 const optional:number[]=[1,2,3];
 if(optional.slice(-2,missingEnd(optional)).join(',')!=='3,4')return 3;
 const left:number[]=[1,2];
 let forward='';
 const total=left.reduce((acc,n,i,a)=>{
   forward+=String(i)+':'+String(n)+';';
   if(i===0)a.push(4);
   return acc+n;
 },append(left,3,10));
 if(total!==16 || forward!=='0:1;1:2;2:3;' || left.length!==4)return 4;
 const right:number[]=[1,2];
 let reverse='';
 const reverseTotal=right.reduceRight((acc,n,i,a)=>{
   reverse+=String(i)+':'+String(n)+';';
   if(i===2)a.push(4);
   return acc+n;
 },append(right,3,10));
 if(reverseTotal!==16 || reverse!=='2:3;1:2;0:1;' || right.length!==4)return 5;
 const empty:number[]=[];
 if(empty.reduce((acc,n)=>acc+n,append(empty,7,1))!==8)return 6;
 return 0;
}

interface RankedItem { rank:number; label:string }
export function sortUndefined():number {
 const dense:(number|undefined)[]=[undefined,3,undefined,1];
 const alias=dense;
 let calls=0;
 const sorted=dense.sort((a,b)=>{
   if(a===undefined || b===undefined)throw new Error('undefined comparator argument');
   calls++;
   return a-b;
 });
 if(sorted!==alias || calls!==1 || dense[0]!==1 || dense[1]!==3 ||
dense[2]!==undefined || dense[3]!==undefined)return 1;
 const original:(number|undefined)[]=[undefined,3,undefined,1];
 calls=0;
 const copied=original.toSorted((a,b)=>{
   if(a===undefined || b===undefined)throw new Error('undefined comparator argument');
   calls++;
   return a-b;
 });
 if(copied===original || calls!==1 || original[0]!==undefined || original[1]!==3 ||
copied[0]!==1 || copied[1]!==3 || copied[2]!==undefined || copied[3]!==undefined)return 2;
 const words:(string|undefined)[]=[undefined,'z','a',undefined];
 if(words.sort().join(',')!=='a,z,,')return 3;
 const nullable:(number|null|undefined)[]=[undefined,null,2,1];
 let sawNull=false;
 nullable.sort((a,b)=>{
   if(a===undefined || b===undefined)throw new Error('undefined nullable argument');
   if(a===null || b===null)sawNull=true;
   return (a===null ? -1 : a)-(b===null ? -1 : b);
 });
 if(!sawNull || nullable[0]!==null || nullable[1]!==1 || nullable[2]!==2 ||
nullable[3]!==undefined)return 4;
 const first:RankedItem={rank:1,label:'a'};
 const second:RankedItem={rank:1,label:'b'};
 const items:(RankedItem|undefined)[]=[undefined,first,second];
 const stable=items.toSorted((a,b)=>{
   if(a===undefined || b===undefined)throw new Error('undefined object');
   return a.rank-b.rank;
 });
 if(stable[0]!==first || stable[1]!==second || items[1]!==first)return 5;
 const absent:(number|undefined)[]=[undefined,undefined];
 absent.sort((a,b)=>{
   if(a===undefined || b===undefined)throw new Error('unreachable comparator');
   return a-b;
 });
 return 0;
}

export function canonicalKeys():number {
 const map=new Map<number,number>([[-0,1],[2,2]]);
 map.set(-0,3);
 let trace='';
 map.forEach((value,key)=>{trace+=String(1/key)+':'+String(value)+';';});
 if(map.size!==2 || trace!=='Infinity:3;0.5:2;')return 1;
 const updates=new Map<number,number>([[0,1]]);
 updates.set(-0,2);
 for(const key of updates.keys())if(1/key!==Infinity)return 2;
 updates.delete(-0);
 updates.set(-0,4);
 for(const [key,value] of updates)if(1/key!==Infinity || value!==4)return 3;
 const set=new Set<number>([-0,2]);
 set.add(-0);
 trace='';
 set.forEach((value,key)=>{trace+=String(1/value)+':'+String(1/key)+';';});
 if(set.size!==2 || trace!=='Infinity:Infinity;0.5:0.5;')return 4;
 const inserted=new Set<number>();
 inserted.add(-0);
 for(const key of inserted.values())if(1/key!==Infinity)return 5;
 const optional=new Map<number|null|undefined,number>([[-0,1],[null,2],[undefined,3]]);
 if(optional.size!==3 || optional.get(null)!==2 || optional.get(undefined)!==3)return 6;
 for(const key of optional.keys())if(key!==null && key!==undefined && 1/key!==Infinity)return 7;
 const mixed=new Map<number|string,number>([[-0,1],['0',2]]);
 if(mixed.size!==2 || mixed.get('0')!==2)return 8;
 for(const key of mixed.keys())if(typeof key==='number' && 1/key!==Infinity)return 9;
 const numbers=new Set<number>([NaN,NaN,-0,0]);
 if(numbers.size!==2 || !numbers.has(NaN) || !numbers.has(-0))return 10;
 return 0;
}

function grow(set:Set<number>, other:Set<number>):Set<number> {set.add(4);return other;}
export function intersectionOrder():number {
 const receiver=new Set<number>([3,1,2]);
 const smaller=new Set<number>([2,3]);
 if(Array.from(receiver.intersection(smaller)).join(',')!=='2,3')return 1;
 const larger=new Set<number>([2,3,4,1]);
 if(Array.from(receiver.intersection(larger)).join(',')!=='3,1,2')return 2;
 const equal=new Set<number>([2,1,3]);
 if(Array.from(receiver.intersection(equal)).join(',')!=='3,1,2')return 3;
 if(Array.from(receiver.intersection(grow(receiver,equal))).join(',')!=='2,1,3')return 4;
 if(Array.from(receiver).join(',')!=='3,1,2,4' || Array.from(equal).join(',')!=='2,1,3')return 5;
 equal.delete(1);
 equal.add(1);
 if(Array.from(receiver.intersection(equal)).join(',')!=='2,3,1')return 6;
 if(Array.from(receiver.difference(equal)).join(',')!=='4')return 7;
 if(Array.from(smaller.union(equal)).join(',')!=='2,3,1')return 8;
 if(Array.from(smaller.symmetricDifference(new Set<number>([3,1]))).join(',')!=='2,1')return 9;
 return 0;
}

interface Item { value:number }
export function liveGrouping():number {
 const items:Item[]=[{value:1},{value:2}];
 const alias=items;
 let trace='';
 const groups=Map.groupBy(items,(item,index)=>{
   trace+=String(index)+':'+String(item.value)+';';
   if(index===0)alias.push({value:3});
   return index%2===0 ? -0 : 1;
 });
 if(trace!=='0:1;1:2;2:3;' || groups.size!==2)return 1;
 const zero=groups.get(0);
 if(zero===undefined || zero.length!==2 || zero[0]!==items[0] || zero[1]!==items[2])return 2;
 zero[0].value=9;
 if(alias[0].value!==9)return 3;
 trace='';
 for(const key of groups.keys())trace+=String(1/key)+';';
 if(trace!=='Infinity;1;')return 4;
 const shrinking:number[]=[1,2,3];
 let visits=0;
 const short=Map.groupBy(shrinking,(value,index)=>{
   visits++;
   if(index===0)shrinking.pop();
   return 0;
 });
 const group=short.get(0);
 if(visits!==2 || group===undefined || group.join(',')!=='1,2')return 5;
 const optional:(number|undefined)[]=[undefined,1];
 visits=0;
 const absent=Map.groupBy(optional,(value,index)=>{visits++;return value===undefined ? 0 : 1;});
 const missing=absent.get(0);
 if(visits!==2 || missing===undefined || missing.length!==1 || missing[0]!==undefined)return 6;
 const empty:number[]=[];
 if(Map.groupBy(empty,(value,index)=>{throw new Error('empty callback');return 0;}).size!==0)return 7;
 return 0;
}
"#,
        &[
            ("argumentOrder", &[], 0.0),
            ("sortUndefined", &[], 0.0),
            ("canonicalKeys", &[], 0.0),
            ("intersectionOrder", &[], 0.0),
            ("liveGrouping", &[], 0.0),
        ],
    )
}
