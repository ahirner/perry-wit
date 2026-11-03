
import {readFile,writeFile} from 'node:fs/promises';
export async function run(x:number):Promise<{value:number,trace:number[]}> {
  const payload="é\u0000漢𝄞😀\nline2";
  const trace:number[]=[];
  await writeFile('dynamic-utf8.txt',payload);
  const first=readFile('dynamic-utf8.txt','utf8');
  const second=readFile('fixture.txt','utf8');
  const restored=await first;
  const fixture=await second;
  if(restored!==payload||fixture!=="é\u0000abc")throw 91;
  const encoded=new TextEncoder().encode(restored);
  trace.push(encoded.length);
  for(let i=0;i<encoded.length;i++){const byte=encoded[i];if(byte===undefined)throw 92;trace.push(byte);}
  return {value:x,trace};
}
