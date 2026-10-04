interface Options { method?: string; headers?: Headers; body?: string; }
function noOptions(options: Options): undefined { options.method="GET"; return undefined; }
export async function run(url: string): Promise<string> {
    const options: Options = {};
    const first = await (await fetch(url,options)).text();
    options.method = "post";
    options.headers = new Headers({"x-value":"before"});
    options.body = "body";
    const pending = fetch(url,options);
    options.headers = undefined;
    options.body = undefined;
    const second = await (await pending).text();
    const third = await (await fetch(url,options)).text();
    const empty = new Headers(noOptions(options));
    if(empty.has("x") || options.method !== "GET") return "constructor argument effects";
    options.method = "POST";
    const effects = await (await fetch(url,noOptions(options))).text();
    if(effects !== "GET::" || options.method !== "GET") return "fetch argument effects";
    const absent: Options = {method:undefined,headers:undefined,body:undefined};
    const fourth = await (await fetch(url,absent)).text();
    const fifth = await (await fetch(url,undefined)).text();
    const sixth = await (await fetch(url,{body:null})).text();
    const explicit: {method:string|undefined;body:string|undefined} = {method:undefined,body:undefined};
    const seventh = await (await fetch(url,explicit)).text();
    let checks = 0;
    try {
        const invalid = fetch(url,{headers:{"bad name":"x"}});
        checks++;
        try { await invalid; } catch { checks++; }
        try { await invalid; } catch { checks++; }
    } catch { return "synchronous rejection"; }
    if(checks !== 3) return "invalid promise";
    return first+"|"+second+"|"+third+"|"+fourth+"|"+fifth+"|"+sixth+"|"+seventh;
}
