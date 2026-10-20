use crate::{core_program::Program, node_oracle::NodeOracle};

#[test]
fn same_value_evaluates_once_and_preserves_optional_numbers() -> anyhow::Result<()> {
    let source = r#"
let calls = 0;
function text(): string { calls++; return 'value'; }
export function run(x: number): number {
    calls = 0;
    const equal = Object.is(text(), text());
    if (!equal || calls !== 2) return 1;
    const a: number | undefined = x === 0 ? -0 : undefined;
    const b: number | undefined = x === 0 ? 0 : undefined;
    if (Object.is(a, b) !== (x !== 0)) return 2;
    if (Object.is(a, 0) || Object.is(0, a)) return 3;
    if (!Object.is(a, a) || !Object.is(NaN, NaN)) return 4;
    const nan: number | undefined = x === 0 ? NaN : undefined;
    if (Object.is(nan, NaN) !== (x === 0)) return 5;
    const nullable: number | null = x === 0 ? -0 : null;
    if (Object.is(nullable, 0) || !Object.is(nullable, nullable)) return 6;
    return 0;
}
"#;
    let oracle = NodeOracle::typescript(source, "console.log(run(Number(process.argv[2])));")?;
    let mut program = Program::checked(source, "same-value.ts");
    for input in [0.0, 1.0] {
        let expected = oracle.run(&[&input.to_string()])?.trim().parse::<f64>()?;
        assert_eq!(expected, 0.0, "Node regression fixture");
        assert_eq!(program.number("run", input), expected);
    }
    Ok(())
}

#[test]
fn record_enumeration_preserves_instance_order_and_presence() -> anyhow::Result<()> {
    let source = r#"
interface Pair { z: number; a: number; missing?: number }
function check(value: Pair, names: string, values: string): number {
    if (Object.keys(value).join(',') !== names) return 1;
    if (Object.values(value).join(',') !== values) return 2;
    const entries = Object.entries(value);
    let keys = ''; let vals = '';
    for (const entry of entries) { keys += entry[0] + ','; vals += entry[1] + ','; }
    if (keys !== names + ',' || vals !== values + ',') return 3;
    return 0;
}
export function run(): number {
    const first: Pair = {z: 1, a: 2}; const second: Pair = {a: 2, z: 1};
    if (check(first, 'z,a', '1,2') !== 0) return 1;
    if (check(second, 'a,z', '2,1') !== 0) return 2;
    first.missing = undefined;
    if (Object.keys(first).join(',') !== 'z,a,missing') return 3;
    delete first.missing;
    first.missing = 3;
    if (check(first, 'z,a,missing', '1,2,3') !== 0) return 4;
    const numbered = {'10': 10, z: 1, '2': 2, a: 3};
    if (Object.keys(numbered).join(',') !== '2,10,z,a') return 5;
    const large = {z: 0, y: 1, x: 2, w: 3, v: 4, u: 5, t: 6, s: 7, r: 8};
    if (Object.keys(large).join(',') !== 'z,y,x,w,v,u,t,s,r') return 6;
    const decoded = JSON.parse<{z:number,y:number,x:number,w:number,v:number,u:number,t:number,s:number,r:number}>('{"x":2,"z":0,"r":8,"y":1,"w":3,"v":4,"u":5,"t":6,"s":7}');
    if (Object.keys(decoded).join(',') !== 'x,z,r,y,w,v,u,t,s') return 8;
    if (Object.values(decoded).join(',') !== '2,0,8,1,3,4,5,6,7') return 9;
    const tuple = JSON.parse<[number,number]>('[1,2]');
    if (Object.keys(tuple).join(',') !== '0,1') return 16;
    const partial = JSON.parse<{a:unknown,b:unknown}>('{"b":2}');
    if (Object.keys(partial).join(',') !== 'b') return 12;
    if (JSON.stringify(Object.values(partial)) !== '[2]') return 13;
    if (JSON.stringify(Object.entries(partial)) !== '[["b",2]]') return 14;
    partial.a = 3;
    if (Object.keys(partial).join(',') !== 'b,a') return 17;
    if (JSON.stringify(Object.entries(partial)) !== '[["b",2],["a",3]]') return 18;
    const empty = JSON.parse<{only:unknown}>('{}');
    if (Object.keys(empty).length !== 0) return 15;
    const wide = {/*FIELDS*/};
    if (Object.keys(wide).join(',') !== '/*NAMES*/') return 10;
    if (Object.values(wide).join(',') !== '/*VALUES*/') return 11;
    const collision1 = {x: 1}; const collision2 = {x: 'two'}; const collision3 = {x_2: 3};
    if (collision1.x + collision2.x.length + collision3.x_2 !== 7) return 7;
    return 0;
}
"#;
    let names = (0..70)
        .rev()
        .map(|index| format!("p{index}"))
        .collect::<Vec<_>>();
    let values = (0..70)
        .rev()
        .map(|index| index.to_string())
        .collect::<Vec<_>>();
    let fields = names
        .iter()
        .zip(&values)
        .map(|(name, value)| format!("{name}:{value}"))
        .collect::<Vec<_>>();
    let source = source
        .replace("/*FIELDS*/", &fields.join(","))
        .replace("/*NAMES*/", &names.join(","))
        .replace("/*VALUES*/", &values.join(","));
    let oracle = NodeOracle::typescript(&source, "console.log(run());")?;
    let expected = oracle.run(&[])?.trim().parse::<f64>()?;
    let mut program = Program::checked(&source, "record-order.ts");
    program.stress_collection(true);
    assert_eq!(expected, 0.0, "Node regression fixture");
    let (results, _) = program.call("run", &[]);
    assert_eq!(results[0].unwrap_f64(), expected);
    if let Some(status) = results.get(1) {
        assert_eq!(status.unwrap_i32(), 0);
    }
    Ok(())
}
