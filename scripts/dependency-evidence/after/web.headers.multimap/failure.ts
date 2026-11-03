
export function run(x:number):{value:number,trace:number[]} {
  const input=new Uint8Array([]);
  const trace:number[]=[];
  
const headers=new Headers({'X-Test':""});
headers.append('x-test','second');
const joined=headers.get('X-Test');if(joined===null)throw 90;
const encoded=new TextEncoder().encode(joined);
for(let i=0;i<encoded.length;i++){const byte=encoded[i];if(byte===undefined)throw 91;trace.push(byte);}
headers.set('X-Test',' replaced ');
if(headers.get('x-test')!=='replaced'||!headers.has('X-Test'))throw 92;
headers.delete('x-test');if(headers.has('X-Test')||headers.get('X-Test')!==null)throw 93;

  return {value:x,trace};
}
