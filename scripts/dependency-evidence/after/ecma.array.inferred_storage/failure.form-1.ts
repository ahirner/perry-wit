function mark(trace:number[],n:number):number {trace.push(n);return n;}
function observe(trace:number[],text:string|undefined):void {const bytes=new TextEncoder().encode(text);trace.push(-999,bytes.length);for(let i=0;i<bytes.length;i++){const byte=bytes[i];if(byte===undefined)throw 91;trace.push(byte);}}
export function run(x:number):{value:number,trace:number[]} {
  const trace:number[]=[];
  const values:(any)[]=[];
  const alias0=values;
  for(let step=0;step<2;step++) {
    switch(step) {
      case 0: values.push(mark(trace,0)); break;
      case 1: alias0.push(x<0?mark(trace,0):JSON.parse('{"incompatible":true}')); break;
    }
  }
  observe(trace,JSON.stringify(values));observe(trace,JSON.stringify(alias0));trace.push(values.length);
  return {value:x,trace};
}
