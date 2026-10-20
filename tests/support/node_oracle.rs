use crate::core_program::Program;
use anyhow::{Context, Result, ensure};
use std::{io::Write, process::Command};
use wasmtime::Val;

/// Run the same numeric export calls in Node and a memory-checked Wasm instance.
pub(crate) fn assert_numbers(source: &str, cases: &[(&str, &[f64], f64)]) -> Result<()> {
    let mut script = tempfile::Builder::new().suffix(".mts").tempfile()?;
    writeln!(script, "{source}")?;
    for (export, args, _) in cases {
        writeln!(
            script,
            "console.log({export}(...{}));",
            serde_json::to_string(args)?
        )?;
    }
    let output = Command::new("node")
        .arg("--experimental-strip-types")
        .arg(script.path())
        .output()
        .context("Node oracle")?;
    ensure!(
        output.status.success(),
        "Node oracle exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).context("Node oracle stdout is not UTF-8")?;
    let mut lines = stdout.lines();
    let mut program = Program::checked(source, "oracle.ts");
    program.stress_collection(true);
    for (export, args, expected) in cases {
        let oracle = lines
            .next()
            .context("missing Node result")?
            .parse::<f64>()?;
        assert_eq!(oracle, *expected, "Node {export}({args:?})");
        let args: Vec<_> = args.iter().map(|value| Val::F64(value.to_bits())).collect();
        let (results, _) = program.call(export, &args);
        assert_eq!(results[0].unwrap_f64(), oracle, "Wasm {export}({args:?})");
        assert!(matches!(results.len(), 1 | 2), "{export}: {results:?}");
        if let Some(status) = results.get(1) {
            assert_eq!(status.unwrap_i32(), 0, "{export}: unexpected throw");
        }
    }
    ensure!(lines.next().is_none(), "unexpected Node output: {stdout}");
    Ok(())
}
