use anyhow::Result;
use perry_json_helper as json;
use std::process::Command;

#[test]
fn serialization_matches_node_for_unicode_numbers_duplicate_keys_and_property_order() -> Result<()>
{
    let mut inputs: Vec<String> = [
        "null", "true", "false", "-0", "1e400", "-1e400", "1e-400", "1e21", "1e20", "1e-6", "1e-7",
        "2.9802322387695313e-8", "1000000000000000128", "5e-324", "2.2250738585072014e-308",
        r#""🦀\uD83D\uDE00\u0000\b\f\n\r\t\/\\\"""#,
        r#"{"2":"two","10":"ten","1":"one","b":1,"a":2,"b":3,"01":4,"4294967295":5,"4294967294":6,"0":7,"00":8}"#,
        r#"{"\u0061":1,"a":2,"__proto__":{"ok":true},"nested":[null,{},[],false]}"#,
    ].into_iter().map(str::to_owned).collect();
    let mut bits = 0xABCD_ABCD_ABCD_ABCDu64;
    for _ in 0..256 {
        bits = bits
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let number = f64::from_bits(bits);
        if number.is_finite() {
            inputs.push(number.to_string());
        }
    }
    inputs.push(format!("0.{}1", "0".repeat(1000)));
    inputs.push(format!("1{}e-1000", "2".repeat(1000)));
    let reference = Command::new("node").args([
        "--input-type=module", "-e",
        "console.log(JSON.stringify(JSON.parse(process.argv[1]).map(text => JSON.stringify(JSON.parse(text)))))",
        &serde_json::to_string(&inputs)?,
    ]).output()?;
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&reference.stdout)?;
    for (input, expected) in inputs.iter().zip(expected) {
        let mut graph = vec![0; json::measure(input).unwrap()];
        let root = json::populate(input, &mut graph, 4096).unwrap();
        let length = json::measure_serialized(&graph, 4096, root).unwrap();
        let mut result = vec![0; length];
        assert_eq!(
            json::serialize(&graph, 4096, root, &mut result).unwrap(),
            length
        );
        assert_eq!(std::str::from_utf8(&result)?, expected, "{input}");
        assert_eq!(
            json::serialize(&graph, 4096, root, &mut result[..length - 1]),
            Err(json::Error::Capacity)
        );
    }
    Ok(())
}

#[test]
fn serialization_checks_all_graph_ranges_and_kinds() {
    let mut graph = vec![0; json::measure(r#""valid""#).unwrap()];
    let root = json::populate(r#""valid""#, &mut graph, 4096).unwrap();
    for invalid in [0, 4095, u32::MAX] {
        assert_eq!(
            json::measure_serialized(&graph, 4096, invalid),
            Err(json::Error::InvalidGraph)
        );
    }
    graph[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        json::measure_serialized(&graph, 4096, root),
        Err(json::Error::InvalidGraph)
    );
    graph[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        json::measure_serialized(&graph, 4096, root),
        Err(json::Error::InvalidGraph)
    );
}

#[test]
fn parser_validates_complete_json_and_rejects_unpaired_escapes() {
    for input in [
        "",
        " ",
        "null false",
        "nul",
        "True",
        "01",
        "-",
        "1.",
        "1e",
        "[1,]",
        "{\"x\":}",
        "{x:1}",
        "\"a\n\"",
        r#""\x20""#,
        r#""\uD800""#,
        r#""\uDC00""#,
        r#"{"\uD800":1}"#,
    ] {
        assert!(json::measure(input).is_err(), "{input:?}");
    }
    assert!(matches!(
        json::measure(r#""\uD800""#),
        Err(json::Error::UnpairedSurrogate(1))
    ));
    let deep = format!("{}0{}", "[".repeat(128), "]".repeat(128));
    assert!(matches!(json::measure(&deep), Err(json::Error::Depth(_))));
}

#[test]
fn parser_populates_shared_storage_with_exact_types_and_unicode() -> Result<()> {
    fn normalize_numbers(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Number(number) => {
                *value = serde_json::json!(number.as_f64().unwrap())
            }
            serde_json::Value::Array(values) => values.iter_mut().for_each(normalize_numbers),
            serde_json::Value::Object(values) => values.values_mut().for_each(normalize_numbers),
            _ => {}
        }
    }
    fn word(bytes: &[u8], at: usize) -> usize {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }
    fn value(bytes: &[u8], at: usize, base: usize) -> serde_json::Value {
        match word(bytes, at) {
            1 => serde_json::Value::Null,
            2 => serde_json::Value::Bool(word(bytes, at + 8) != 0),
            3 => serde_json::json!(f64::from_le_bytes(
                bytes[at + 8..at + 16].try_into().unwrap()
            )),
            4 => {
                let start = word(bytes, at + 8) - base;
                let end = start + word(bytes, at + 12);
                let text = std::str::from_utf8(&bytes[start..end]).unwrap();
                assert_eq!(word(bytes, at + 16), text.chars().count());
                serde_json::Value::String(text.into())
            }
            kind @ (5 | 6) => {
                let mut child = word(bytes, at + 16);
                let mut values = Vec::new();
                let mut object = serde_json::Map::new();
                for _ in 0..word(bytes, at + 20) {
                    assert!(child >= base);
                    let at = child - base;
                    let item = value(bytes, at, base);
                    if kind == 5 {
                        values.push(item);
                    } else {
                        let key = value(bytes, word(bytes, at + 24) - base, base)
                            .as_str()
                            .unwrap()
                            .to_owned();
                        object.insert(key, item);
                    }
                    child = word(bytes, at + 4);
                }
                assert_eq!(child, 0);
                if kind == 5 {
                    serde_json::Value::Array(values)
                } else {
                    serde_json::Value::Object(object)
                }
            }
            kind => panic!("Invalid node kind {kind}"),
        }
    }
    for input in [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "42.5",
        "1.2345678901234567e-200",
        r#""🦀\uD83D\uDE00\u0000\b\f\n\r\t\/\\\"""#,
        r#"[{},[],null,true,42.5,"ß"]"#,
        r#"{"a":{"b":["é",1,2]},"a":false,"__proto__":null}"#,
    ] {
        let size = json::measure(input).unwrap();
        let mut bytes = vec![0xFF; size];
        let root = json::populate(input, &mut bytes, 1024).unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(input)?;
        normalize_numbers(&mut expected);
        assert_eq!(
            value(&bytes, root as usize - 1024, 1024),
            expected,
            "{input}"
        );
        assert_eq!(
            json::populate(input, &mut bytes[..size - 1], 1024),
            Err(json::Error::Capacity)
        );
    }
    Ok(())
}
