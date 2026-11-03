
async function fail(message:string):Promise<number> {
    throw new TypeError(message, {cause:message});
}
export async function run(x:number):Promise<{value:number,trace:number[]}> {
    const message="";
    const trace:number[]=[];
    const pending=fail(message);
    try {await pending;} catch(original) {
        if(!(original instanceof TypeError)||!(original instanceof Error))throw 90;
        if(original.name!=='TypeError'||original.message!==message||original.cause!==message)throw 91;
        try {await Promise.all([pending,1]);} catch(e) {if(e!==original)throw 92;trace.push(1);}
        try {await Promise.race([pending]);} catch(e) {if(e!==original)throw 93;trace.push(2);}
        const settled=await Promise.allSettled([pending]);
        if(settled[0].status!=='rejected'||settled[0].reason!==original)throw 94;
        try {try {throw original;} finally {trace.push(3);}}
        catch(e) {if(e!==original)throw 95;trace.push(4);}
        if(JSON.stringify(original)!=='{}'||JSON.stringify({...original})!=='{}')throw 96;
        const encoded=new TextEncoder().encode('message:'+original.message);
        for(let i=0;i<encoded.length;i++){const byte=encoded[i];if(byte===undefined)throw 97;trace.push(byte);}
        return {value:x,trace};
    }
    throw 98;
}
