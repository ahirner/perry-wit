
import {readFile,writeFile} from 'node:fs/promises';
export async function run(x:number):Promise<{value:number,trace:number[]}> {
  const first="initial-long-payload-é\u0000漢";
  const second="short";
  const trace:number[]=[];
  await writeFile('roundtrip.txt',first);
  const r1=await readFile('roundtrip.txt','utf8');
  if(r1!==first)throw 90;
  await writeFile('roundtrip.txt',second);
  const r2=await readFile('roundtrip.txt','utf8');
  if(r2!==second)throw 92;
  const encoded1=new TextEncoder().encode(r1);
  const encoded2=new TextEncoder().encode(r2);
  trace.push(encoded1.length);
  trace.push(encoded2.length);
  for(let i=0;i<encoded1.length;i++){const byte=encoded1[i];if(byte===undefined)throw 93;trace.push(byte);}
  for(let i=0;i<encoded2.length;i++){const byte=encoded2[i];if(byte===undefined)throw 93;trace.push(byte);}
  return {value:x,trace};
}
