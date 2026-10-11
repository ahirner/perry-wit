//! Small-component size and initial-memory limits.
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmparser::{Parser, Payload};

#[test]
fn small_components_preserve_size_and_initial_memory() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "world.wit",
        "package perry:baseline; world baseline { export run: func(input: string) -> string; }",
    )?;
    let world = resolve.select_world(&[package], Some("baseline"))?;
    for (name, source, max_bytes) in [
        (
            "identity",
            "export function run(input: string): string { return input; }",
            3047,
        ),
        (
            "append",
            "export function run(input: string): string { return \"TASK_PROCESSED: \" + input; }",
            3446,
        ),
    ] {
        let compiled = compile_for_world(source, name, &resolve, world, true)?;
        let bytes = compiled.stripped.as_ref().unwrap().len() as u64;
        assert!(
            bytes <= max_bytes,
            "{name}: {bytes} bytes exceeds {max_bytes}-byte limit"
        );
        let mut memories = 0;
        for section in Parser::new(0).parse_all(&compiled.core) {
            if let Payload::MemorySection(section) = section? {
                for memory in section {
                    memories += 1;
                    assert_eq!(
                        memory?.initial, 1,
                        "{name}: initial memory must be one page"
                    );
                }
            }
        }
        assert_eq!(memories, 1, "{name}: one linear memory");
    }
    Ok(())
}
