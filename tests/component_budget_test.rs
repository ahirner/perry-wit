//! Frozen small-component budgets, independent of host compilation speed.
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmparser::{Parser, Payload};

#[test]
fn small_components_preserve_size_and_initial_memory() -> Result<()> {
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("../bench/baseline/measurements.json"))?;
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("world.wit", include_str!("../bench/baseline/world.wit"))?;
    let world = resolve.select_world(&[package], Some("baseline"))?;
    for (name, source) in [
        ("identity", include_str!("../bench/baseline/identity.ts")),
        ("append", include_str!("../bench/baseline/append.ts")),
    ] {
        let compiled = compile_for_world(source, name, &resolve, world, true)?;
        let bytes = compiled.stripped.as_ref().unwrap().len() as u64;
        let original = baseline["components"][name]["bytes"].as_u64().unwrap();
        assert!(
            bytes * 10 <= original * 11,
            "{name}: {bytes} bytes, baseline {original}"
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
