//! Chunk boundaries, error recovery and BOM handling for the declared UTF-8 decoder.
use crate::world::Compiler::Standard;
use anyhow::Result;
use wasmtime::Engine;

use crate::{node_oracle::NodeOracle, world};

#[test]
fn incremental_utf8_matches_node_at_every_byte_boundary() -> Result<()> {
    let source = r#"
export function decode(input: Uint8Array, split: number, fatal: boolean, ignore: boolean): string {
    const decoder = new TextDecoder('utf-8', { fatal, ignoreBOM: ignore });
    const alias = decoder;
    const parts: string[] = [];
    try {
        parts.push(decoder.decode(input.subarray(0, split), { stream: true }));
        parts.push(alias.decode(input.subarray(split), { stream: true }));
        parts.push(decoder.decode());
    } catch (error) {
        parts.push('error');
    }
    parts.push(decoder.decode(new Uint8Array([0xef, 0xbb, 0xbf, 65])));
    parts.push(decoder.decode(new Uint8Array([0xef, 0xbb, 0xbf, 66])));
    return JSON.stringify({ parts, encoding: decoder.encoding, fatal: decoder.fatal, ignore: decoder.ignoreBOM });
}
"#;
    let wit = "package test:decoder; world decoder { export decode: func(input: list<u8>, split: f64, fatal: bool, ignore: bool) -> result<string>; }";
    let component = Standard.component(wit, "decoder", "decoder.ts", source)?;
    let engine = Engine::default();
    let (mut store, instance) = world::instantiate(&engine, &component)?;
    let decode = instance
        .get_typed_func::<(&[u8], f64, bool, bool), (std::result::Result<String, ()>,)>(
            &mut store, "decode",
        )?;
    let oracle = NodeOracle::typescript(
        source,
        "const input = new Uint8Array(JSON.parse(process.argv[2]));\nconst values = [];\nfor (let split = 0; split <= input.length; split++) { for (const fatal of [false, true]) { for (const ignore of [false, true]) values.push(JSON.parse(decode(input, split, fatal, ignore))); } }\nconsole.log(JSON.stringify(values));",
    )?;
    for input in [
        b"".as_slice(),
        b"ASCII\0text",
        b"\xef\xbb\xbf\xc3\xa9\xf0\x9f\x99\x82\xef\xbb\xbfZ",
        b"\xe0\x80x\xed\xa0\x80\xf4\x90\x80\x80\xe2\x82",
        b"\xef\xbb",
    ] {
        let expected: Vec<serde_json::Value> =
            serde_json::from_str(&oracle.run(&[&serde_json::to_string(input)?])?)?;
        let mut index = 0;
        for split in 0..=input.len() {
            for fatal in [false, true] {
                for ignore in [false, true] {
                    let (actual,) =
                        decode.call(&mut store, (input, split as f64, fatal, ignore))?;
                    let actual = actual.expect("fixture has no escaping failure");
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&actual)?,
                        expected[index],
                        "input={input:?} split={split} fatal={fatal} ignore={ignore}"
                    );
                    index += 1;
                }
            }
        }
    }
    Ok(())
}
