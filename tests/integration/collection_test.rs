//! Observable array, Map, and Set semantics compared against Node.
use crate::node_oracle::assert_numbers;

#[test]
fn collections_preserve_evaluation_order_and_key_semantics() -> anyhow::Result<()> {
    let error = perry_wit::compile_core(
        "export function run():number { const row={value:1,extra:2}; const mixed:(number|{value:number})[]=[row]; return mixed.length; }",
        "union-layout.ts",
    ).expect_err("union boxing cannot copy a record to a different layout");
    assert!(
        error
            .to_string()
            .contains("preserve their layout and identity")
    );
    assert_numbers(
        r#"
function append(a:number[], value:number, result:number) {a.push(value);return result;}
function shorten(a:number[]) {a.pop();return -2;}
function missingEnd(a:number[]):number|undefined {a.push(4);return undefined;}
export function argumentOrder() {
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
export function sortUndefined() {
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

export function canonicalKeys() {
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

function grow(set:Set<number>, other:Set<number>) {set.add(4);return other;}
export function intersectionOrder() {
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
export function liveGrouping() {
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

export function unknownAbsence() {
 const values=new Map<string,unknown>();
 values.set('undefined',undefined);
 values.set('null',null);
 if(values.size!==2 || !values.has('undefined') || values.has('missing'))return 1;
 if(values.get('undefined')!==undefined || values.get('null')!==null)return 2;
 let trace='';
 values.forEach((value,key)=>{trace+=key+':'+String(value)+';';});
 if(trace!=='undefined:undefined;null:null;')return 3;
 const record:{value:unknown}={value:undefined};
 const array:unknown[]=[undefined,null];
 if(record.value!==undefined || array[0]!==undefined || array[1]!==null)return 4;
 return 0;
}

export function tupleCallbacks() {
 const pairs:[string,number][]=[['a',1],['b',2]];
 const found=pairs.find(([key])=>key==='b');
 if(found===undefined || found[1]!==2)return 1;
 if(pairs.findIndex(([key,value])=>key==='b' && value===2)!==1)return 2;
 if(pairs.find(([key])=>key==='missing')!==undefined)return 3;
 const optional:[string,number|undefined][]=[['a',undefined],['b',0]];
 if(optional.map(([key,value=7])=>key+String(value)).join(',')!=='a7,b0')return 4;
 return 0;
}

function mixedPrimitive(flag:number) {
 if(flag>0)return '雪';
 return 2;
}
export function mixedValues(size:number) {
 const row:Item=JSON.parse('{"value":7,"extra":{"keep":"雪"}}');
 const values:(number|string|boolean|Item|null|undefined)[]=[];
 const alias=values;
 for(let i=0;i<size;i++)alias.push(i);
 values.push(row,'x',true,null,undefined,NaN,-0);
 const held=values[size];
 row.value=11;
 if(held!==row || held.value!==11)return 1;
 held.value=13;
 if(row.value!==13 || values.indexOf(row)!==size || !values.includes(row))return 2;
 const sum=mixedPrimitive(size)+1;
 if(size>0 && sum!=='雪1')return 3;
 if(size===0 && sum!==3)return 3;
 if(values[size+1]+true!=='xtrue' || values[size+2]+'!'!=='true!')return 4;
 if(values[size+2]+2!==3 || values[size+3]+2!==2)return 5;
 if(values[size+4]===null || values[size+3]===undefined)return 6;
 if(values[size+5]===values[size+5] || !values.includes(NaN))return 7;
 if(!Object.is(values[size+6],-0) || Object.is(values[size+6],0))return 8;
 const copied=values.slice();
 values[size]=99;
 if(copied[size]!==row || JSON.stringify(copied[size])!=='{"value":13,"extra":{"keep":"雪"}}')return 9;
 let count=0;
 for(let i=0;i<copied.length;i++){
   const value=copied[i];
   if(typeof value==='number')count++;
   if(typeof value==='string' && value.length===0)return 10;
 }
 if(count!==size+2)return 11;
 const nullable=values[size+3];
 if(typeof nullable==='object' && nullable!==null)return 12;
 const tagged:(number|{tag:string;value:number})[]=[{tag:'number',value:5}];
 const taggedValue=tagged[0];
 if(taggedValue.tag==='number' && taggedValue.value!==5)return 13;
 const numbers=[3];
 values.push(numbers[0],numbers[9]);
 if(values[values.length-2]!==3 || values[values.length-1]!==undefined)return 14;

 return 0;
}

export function dictionaryKeyPrefixes() {
 const keys=['','\0','a','a\0','a\0\0','abcdefgh','abcdefgha','abcdefghb','ABCDABCD','EFGHEFGH','é','雪','😀','abcdé雪a','abcdé雪b'];
 for(let n=1;n<12;n++)keys.push('x'.repeat(n));
 const dictionary:Record<string,number>={};
 for(let i=0;i<keys.length;i++){
   const key=('!'+keys[i]+'?').slice(1,-1);
   dictionary[key]=i+1;
 }
 dictionary['abcdefgha']=99;
 for(let i=keys.length-1;i>=0;i--){
   const key=(':'+keys[i]).slice(1);
   if(dictionary[key] !== (key==='abcdefgha' ? 99 : i+1))return 1;
 }
 if(dictionary['abcdefghc']!==undefined || dictionary['abcdé雪c']!==undefined)return 2;
 if(Object.keys(dictionary).join('|')!==keys.join('|'))return 3;
 return 0;
}
"#,
        &[
            ("argumentOrder", &[], 0.0),
            ("sortUndefined", &[], 0.0),
            ("canonicalKeys", &[], 0.0),
            ("intersectionOrder", &[], 0.0),
            ("liveGrouping", &[], 0.0),
            ("unknownAbsence", &[], 0.0),
            ("tupleCallbacks", &[], 0.0),
            ("dictionaryKeyPrefixes", &[], 0.0),
            ("mixedValues", &[0.0], 0.0),
            ("mixedValues", &[129.0], 0.0),
        ],
    )
}

#[test]
fn sparse_arrays_preserve_presence_and_iteration() -> anyhow::Result<()> {
    assert_numbers(
        r#"
export function presence() {
 const a = [,3,undefined,,1];
 if(a.length!==5 || 0 in a || !(2 in a) || 3 in a || !(4 in a))return 1;
 if(a[0]!==undefined || a.at(3)!==undefined || a[8]!==undefined)return 2;
 if(Object.keys(a).join(',')!=='1,2,4' || a.join('|')!=='|3|||1')return 3;
 let calls=0;
 const mapped=a.map((x,i)=>{calls++;return i;});
 if(calls!==3 || mapped.length!==5 || 0 in mapped || 3 in mapped || mapped[4]!==4)return 4;
 if(a.filter(()=>true).length!==3 || a.indexOf(undefined)!==2 || !a.includes(undefined))return 5;
 const holes = [,,];
 if(holes.length!==2 || Object.keys(holes).length!==0 || holes.indexOf(undefined)!==-1 || !holes.includes(undefined))return 6;
 let visited=0;
 holes.find((v)=>{if(v===undefined)visited++;return false;});
 if(visited!==2 || holes.some(()=>true) || !holes.every(()=>false))return 7;
 const nullable=[,null,undefined];
 if(0 in nullable || nullable[0]!==undefined || nullable[1]!==null || !(2 in nullable))return 8;
 const nested=[undefined,[,1],,[,2,,]];
 if(nested.flat().join(',')!==',1,2' || nested.flat().length!==3 || !(0 in nested.flat()))return 9;
 return 0;
}

export function copies() {
 const a:(number|undefined)[]=[,3,undefined,,1];
 const sliced=a.slice(0,4);
 if(sliced.length!==4 || 0 in sliced || 3 in sliced || !(2 in sliced))return 1;
 const concat=a.concat(a);
 if(concat.length!==10 || 0 in concat || 5 in concat || !(7 in concat))return 2;
 const pushed:(number|undefined)[]=[];
 pushed.push(...a);
 if(!(0 in pushed) || pushed.length!==5)return 3;
 const flat=a.flat(0);
 if(flat.length!==3 || flat[0]!==3 || flat[1]!==undefined)return 4;
 const segments:number[][]=[[4],,[5]];
 const spreadConcat=a.concat(...segments);
 if(spreadConcat.length!==8 || !(6 in spreadConcat) || spreadConcat[6]!==undefined || spreadConcat[7]!==5)return 5;
 return 0;
}
export function mutation() {
 const a:(number|undefined)[]=[,3,undefined,,1];
 const alias=a;
 a.sort((a,b)=>a-b);
 if(a!==alias || a[0]!==1 || a[1]!==3 || !(2 in a) || 3 in a || 4 in a)return 1;
 a.reverse();
 if(0 in a || 1 in a || !(2 in a) || a[3]!==3 || a[4]!==1)return 2;
 a[0]=9;
 if(!(0 in a) || a[0]!==9)return 3;
 delete a[3];
 if(3 in alias || alias.length!==5)return 4;
 a.push(7);
 if(!(5 in a) || a.pop()!==7 || a.length!==5)return 5;
 const b:(number|undefined)[]=[,4,,2];
 if(b.reduce((a,b)=>a+b)!==6 || b.reduceRight((a,b)=>a-b)!==-2)return 6;
 let caught=0;
 try{[,,].reduce((a,b)=>a+b);}catch(e){if(e instanceof TypeError)caught++;}
 if(caught!==1)return 7;
 let calls=0;
 const c:(number|undefined)[]=[1,2,3];
 const mapped=c.map((v,i)=>{calls++;if(i===0)delete c[1];return v;});
 if(calls!==2 || mapped.length!==3 || 1 in mapped)return 8;
 try { delete a['length']; return 9; } catch(e) { if(!(e instanceof TypeError))return 10; }
 if(false in a || null in a)return 11;
 return 0;
}
export function materialization() {
 const mapped:(number|undefined)[]=[,1,,2].map(x=>x*2);
 const spread=[...mapped];
 const from=Array.from(mapped);
 const reverse=mapped.toReversed();
 const sorted=mapped.toSorted((a,b)=>a-b);
 const replaced=mapped.with(1,9);
 if(!(0 in spread) || spread[0]!==undefined || !(0 in from))return 1;
 if(reverse[0]!==4 || reverse[1]!==undefined || !(1 in reverse))return 2;
 if(sorted[0]!==2 || sorted[1]!==4 || sorted[2]!==undefined || !(2 in sorted))return 3;
 if(replaced[1]!==9 || replaced[0]!==undefined || !(0 in replaced))return 4;
 let count=0;
 for(const item of mapped){if(item===undefined)count++;}
 if(count!==2)return 5;
 const entries=Array.from(mapped.entries());
 if(entries[0][0]!==0 || entries[0][1]!==undefined)return 6;
 if(Object.values(mapped).join(',')!=='2,4')return 7;
 return 0;
}
function throughAlias(values:number[], erase:boolean) {
 if(erase)delete values[0];
 return values;
}
export function aliases(flag:number) {
 const dense:number[]=[11,12];
 const sparse:number[]=[21,22];
 delete sparse[0];
 const chosen=flag===0?dense:sparse;
 const record={values:chosen};
 const alias=throughAlias(record.values, flag===2);
 const copied=alias.concat();
 const joined=dense.concat(dense,copied);
 const missing=flag!==0;
 if((0 in alias)===missing || (0 in copied)===missing || (4 in joined)===missing)return 1;
 if(missing && (alias[0]!==undefined || copied[0]!==undefined || joined[4]!==undefined))return 2;
 const materialized:(number|undefined)[]=[...copied];
 if(!(0 in materialized) || materialized.length!==2)return 3;
 if(dense[0]!==11 || !(0 in dense))return 4;
 return 0;
}
export function references() {
 const a:(string|undefined)[]=[,'a',,'b'];
 for(let i=0;i<80;i++)a.push('x'+i);
 const b=a.concat(a);
 if(b.length!==168 || 0 in b || 84 in b || b[85]!=='a' || b[167]!=='x79')return 1;
 b.reverse();
 if(b[0]!=='x79' || 167 in b || 83 in b)return 2;
 const c=structuredClone(a);
 if(c===a || 0 in c || 2 in c || c[1]!=='a' || c.length!==84)return 3;
 const short:(string|undefined)[]=[,'a',undefined];
 if(JSON.stringify(short)!=='[null,"a",null]')return 4;
 const obj={items:short};
 if(JSON.stringify(obj)!=='{"items":[null,"a",null]}')return 5;
 if(Object.keys(short).join(',')!=='1,2')return 6;
 short.pop();short.pop();short.pop();short.push('new');
 if(short[0]!=='new' || !(0 in short))return 7;
 const record={name:'record'};
 const records:({name:string}|undefined)[]=[,record,undefined];
 const copy=records.slice();
 if(0 in copy || copy[1]!==record || !(2 in copy))return 8;
 const mixed:(number|string|undefined)[]=[,1,'word',undefined];
 if(JSON.stringify(mixed)!=='[null,1,"word",null]' || mixed[0]!==undefined)return 9;
 const integers:(bigint|undefined)[]=[,1n,2n];
 if(integers[0]!==undefined || integers.toReversed()[2]!==undefined)return 10;
 const flags:boolean[]=[,true,false];
 if(flags[0]!==undefined || flags.includes(undefined)!==true || flags.indexOf(undefined)!==-1)return 11;
 const json:(string|null)[]=JSON.parse(JSON.stringify([,'a',undefined]));
 if(!(0 in json) || json[0]!==null || json[2]!==null)return 12;
 const cause:(string|undefined)[]=[,'a',undefined];
 const err=new Error('sparse',{cause});
 if(!Array.isArray(err.cause) || Object.keys(err.cause).join(',')!=='1,2')return 13;
 return 0;
}
"#,
        &[
            ("presence", &[], 0.0),
            ("copies", &[], 0.0),
            ("mutation", &[], 0.0),
            ("materialization", &[], 0.0),
            ("aliases", &[0.0], 0.0),
            ("aliases", &[1.0], 0.0),
            ("aliases", &[2.0], 0.0),
            ("references", &[], 0.0),
        ],
    )
}

#[test]
fn indexed_deletion_preserves_length_and_write_bounds() {
    let source = r#"
type Values=number[];
function erase(values:Values) { delete values[1]; }
export function run() {
 const values:Values=[10,20,30];
 const alias=values;
 erase(values);
 if(values.length!==3 || 1 in alias || alias[1]!==undefined)return 1;
 if(values.map(x=>x+1).length!==3 || 1 in values.map(x=>x+1))return 2;
 // A number[] copy cannot hold the hole as undefined.
 let typed=0;
 try{typed-=[...values].length;}catch(e){if(e instanceof TypeError)typed++;}
 try{typed-=values.toReversed().length;}catch(e){if(e instanceof TypeError)typed++;}
 const segments:number[][]=[values,values];
 delete segments[0];
 try{typed-=[1].concat(...segments).length;}catch(e){if(e instanceof TypeError)typed++;}
 for(const index of [1,9,0.5]){try{const read:number=values[index];typed-=read;}catch(e){if(e instanceof TypeError)typed++;}}
 const widened:(number|undefined)[]=[...values];
 if(typed!==6 || widened[1]!==undefined || !(1 in widened))return 7;
 if('01' in values || '-0' in values || !(0 in values) || !('length' in values))return 3;
 delete values['01'];
 delete values[-1];
 delete values[9];
 if(values.length!==3 || values[0]!==10)return 4;
 let caught=0;
 for(const index of [-1,1.5,4,Infinity,NaN]) {
   try{values[index]=99;}catch(e){if(e instanceof RangeError)caught++;}
 }
 if(caught!==5 || values.length!==3 || 1 in values)return 5;
 values[1]=7;
 values[3]=40;
 if(!(1 in alias) || values.length!==4 || values[3]!==40)return 6;
 return 0;
}
"#;
    for checked in [false, true] {
        let mut program = if checked {
            crate::core_program::Program::checked(source, "deletion.ts")
        } else {
            crate::core_program::Program::new(source)
        };
        if checked {
            program.stress_collection(true);
        }
        let (results, _) = program.call("run", &[]);
        assert_eq!(results[0].unwrap_f64(), 0.0);
        if let Some(status) = results.get(1) {
            assert_eq!(status.unwrap_i32(), 0);
        }
    }
    // The only deletion reaches the array through a tuple element.
    let tuple = "export function run(){ const pair:[number[],string]=[[4,5,6],'x'];
 const erase=(target:[number[],string])=>{ delete target[0][0]; };
 erase(pair);
 return 0 in pair[0] ? -1 : pair[0].length; }";
    let (results, _) = crate::core_program::Program::new(tuple).call("run", &[]);
    assert_eq!(results[0].unwrap_f64(), 3.0);
}
