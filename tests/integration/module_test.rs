//! Static module graphs: relative imports, namespaces and re-exports resolve at compile time.
use anyhow::Result;

use crate::core_program::Program;

#[test]
fn static_modules_resolve_aliases_namespaces_and_reexports() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("left.ts"),
        "export function value(x: number): number { const values = new Map<string, number>(); values.set('x', x); return values.get('x') + 1; }",
    )?;
    std::fs::write(
        directory.path().join("right.ts"),
        "export function value(x: number): number { const values = new Set<number>([x]); return values.size * x * 2; }",
    )?;
    let entry = directory.path().join("main.ts");
    let entry = entry.to_string_lossy();
    for (source, expected) in [
        (
            "import { value as left } from './left'; import * as right from './right.js'; export function run(x: number): number { return right.value(left(x)); }",
            12.0,
        ),
        ("export { value as run } from './left.js';", 6.0),
    ] {
        let mut program = Program::from_wasm(perry_wit::compile_core(source, &entry)?);
        for marker in ["perry_constructor:", "$$perry", "arrayRecord"] {
            assert!(
                !program
                    .wasm
                    .windows(marker.len())
                    .any(|bytes| bytes == marker.as_bytes())
            );
        }
        assert_eq!(program.number("run", 5.0), expected, "{source}");
    }
    Ok(())
}
