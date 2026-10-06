export async function run(authority: string, path: string): Promise<{ok:true,value:string}|{ok:false,error:number}> {
  try {
  const response = await fetch("http://" + authority + path, { headers: { accept: "application/json" } });
  const bytes = await readBounded(response, 65536);
  if (response.status !== 200) throw 400;
  const mediaType = response.headers.get("content-type");
  if (mediaType === null || mediaType.includes(",")) throw 401;
  if (mediaType !== "application/json") throw 402;
  const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  JSON.parse(text);
  return {ok:true,value:text};
  } catch(error) { if(typeof error==='number') return {ok:false,error};throw error; }
}
