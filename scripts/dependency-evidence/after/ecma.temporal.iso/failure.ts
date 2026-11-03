
export function run(x:number):{value:number,trace:number[]} {
  const input=new Uint8Array([]);
  const trace:number[]=[];
  
const instant=Temporal.Instant.fromEpochMilliseconds(-1);
const date=Temporal.PlainDate.from('2024-02-28').add({days:1});
const dateTime=Temporal.PlainDateTime.from('2024-02-28T12:30:00.123456789').add({days:1});
trace.push(instant.epochMilliseconds);
const encoded=new TextEncoder().encode(instant.toString()+'|'+date.toString()+'|'+dateTime.toString()+'|'+date.calendarId+'|'+dateTime.calendarId);
for(let i=0;i<encoded.length;i++){const byte=encoded[i];if(byte===undefined)throw 90;trace.push(byte);}

  return {value:x,trace};
}
