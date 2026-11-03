function mark(trace:number[],n:number):number {trace.push(n);return n;}
function observe(trace:number[],text:string|undefined):void {const bytes=new TextEncoder().encode(text);trace.push(-999,bytes.length);for(let i=0;i<bytes.length;i++){const byte=bytes[i];if(byte===undefined)throw 91;trace.push(byte);}}
export function run(x:number):{value:number,trace:number[]} {
  const trace:number[]=[];
  const values:any={};
  const alias0=values;
  const alias1=alias0;
  const alias2=alias1;
  for(let step=0;step<4;step++) {
    switch(step) {
      case 0: values['field_'+0]=mark(trace,0); break;
      case 1: alias0['field_'+1]=mark(trace,1); break;
      case 2: alias1['field_'+0]=mark(trace,-2); break;
      case 3: alias2['field_'+1]=mark(trace,3); break;
    }
  }
  observe(trace,JSON.stringify(values));observe(trace,JSON.stringify(alias2));trace.push(Object.keys(values).length);
  return {value:x,trace};
}
