export async function run(base: string): Promise<string> {
    const source = base + "/a/./x/../%2e/b/%2e%2e/é?key='{}#ignored";
    const first = await fetch(source);
    const target = await first.text();
    const second = await fetch(base + "/a\\x\\..\\y/..//z?quoted=\"x\"&raw=`{}");
    const secondTarget = await second.text();
    if(first.url !== base + "/a/%C3%A9?key=%27{}") return "first URL";
    if(second.url !== base + "/a//z?quoted=%22x%22&raw=`{}") return "second URL";
    return target + "|" + secondTarget;
}
