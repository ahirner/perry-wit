
import {writeFile,stat,readdir,unlink} from 'node:fs/promises';
export async function run(x:number):Promise<{value:number,trace:number[]}> {
  const input=new Uint8Array([0,1,255,128,42]);
  const text="extra\u0000é";
  const textBytes=new TextEncoder().encode(text);
  const trace:number[]=[];
  await writeFile('alpha.bin',input);
  await writeFile('beta.txt',text);
  try {
    const fInfo=await stat('fixture.txt');
    const aInfo=await stat('alpha.bin');
    const bInfo=await stat('beta.txt');
    const names=await readdir('.');
    if(!fInfo.isFile()||fInfo.isDirectory()||fInfo.size!==6)throw 93;
    if(!aInfo.isFile()||aInfo.isDirectory()||aInfo.size!==input.length)throw 94;
    if(!bInfo.isFile()||bInfo.isDirectory()||bInfo.size!==textBytes.length)throw 95;
    if(names.length!==3)throw 96;
    trace.push(fInfo.size);
    trace.push(aInfo.size);
    trace.push(bInfo.size);
    trace.push(names.length);
  } finally {
    await unlink('alpha.bin');
    await unlink('beta.txt');
  }
  const remaining=await readdir('.');
  if(remaining.length!==1||remaining[0]!=='fixture.txt')throw 97;
  return {value:x,trace};
}
