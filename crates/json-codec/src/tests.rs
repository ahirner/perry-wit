use super::*;
extern crate std;
use std::{string::String, vec};

#[test]
fn formatted_graphs_preserve_filter_order_and_duplicate_values() {
    let text = r#"{"a":1,"2":2,"b":[{"a":3,"2":4},[],{}],"a":5}"#;
    let mut storage = vec![0; measure(text).unwrap()];
    let root = populate(text, &mut storage, 8).unwrap();
    let graph = Graph {
        bytes: &storage,
        base: 8,
    };
    for (properties, gap, expected) in [
        (
            None,
            "  ",
            "{\n  \"2\": 2,\n  \"a\": 5,\n  \"b\": [\n    {\n      \"2\": 4,\n      \"a\": 3\n    },\n    [],\n    {}\n  ]\n}",
        ),
        (
            Some(&["b", "a", "2"][..]),
            "",
            r#"{"b":[{"a":3,"2":4},[],{}],"a":5,"2":2}"#,
        ),
        (Some(&[][..]), "  ", "{}"),
        (Some(&["missing", "a"][..]), "é", "{\né\"a\": 5\n}"),
    ] {
        let options = StringifyOptions {
            properties,
            gap: Ok(gap),
        };
        let size = measure_tree_with_options(&graph, root, &options).unwrap();
        assert_eq!(size, expected.len());
        let mut output = vec![0; size];
        assert_eq!(
            serialize_tree_with_options(&graph, root, &mut output, &options),
            Ok(size)
        );
        assert_eq!(String::from_utf8(output).unwrap(), expected);
        let mut short = vec![0; size - 1];
        assert_eq!(
            serialize_tree_with_options(&graph, root, &mut short, &options),
            Err(Error::Capacity)
        );
    }
}

#[test]
fn deep_graphs_serialize_and_container_cycles_fail() {
    let text = std::format!("{}0{}", "[".repeat(127), "]".repeat(127));
    let mut storage = vec![0; measure(&text).unwrap()];
    let root = populate(&text, &mut storage, 8).unwrap();
    let mut output = vec![0; text.len()];
    assert_eq!(serialize(&storage, 8, root, &mut output), Ok(text.len()));
    assert_eq!(output, text.as_bytes());

    let at = (root - 8) as usize;
    storage[at + 16..at + 20].copy_from_slice(&root.to_le_bytes());
    assert_eq!(measure_serialized(&storage, 8, root), Err(Error::Cycle));
    assert_eq!(serialize(&storage, 8, root, &mut output), Err(Error::Cycle));
}
