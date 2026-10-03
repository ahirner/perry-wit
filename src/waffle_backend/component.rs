//! WebAssembly Component Model framing for WAFFLE core modules, supporting WASI 0.3 (P3).

use crate::waffle_backend::resolve::{ResolvedContract, ResolvedInputKind, TypedIntrinsic};
use anyhow::{Context, Result, ensure};

/// The WASI 0.3 monotonic clock adapter for async stack suspension.
pub(crate) const P3_CLOCK_ADAPTER: &str = r#"
  (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
    (export "wait-for" (func async (param "how-long" u64)))))
  (alias export $clock "wait-for" (func $wait-for))
  (core func $wait-for (canon lower (func $wait-for)))
  (core module $delay
    (import "wasi" "wait-for" (func $wait-for (param i64)))
    (func (export "wait-for") (param $milliseconds f64)
      (if (f64.lt (local.get $milliseconds) (f64.const 0)) (then unreachable))
      (call $wait-for (i64.trunc_f64_u
        (f64.mul (local.get $milliseconds) (f64.const 1000000))))))
  (core instance $delay (instantiate $delay
    (with "wasi" (instance (export "wait-for" (func $wait-for))))))
"#;

/// Frame core Wasm bytecode into a Component Model component.
pub(crate) fn frame_component(
    core_wasm: &[u8],
    contract: &ResolvedContract,
) -> Result<(String, Vec<u8>)> {
    ensure!(
        contract.input_kind != ResolvedInputKind::ByteStream,
        "ByteStream componentization is unsupported until its canonical ABI adapter is implemented"
    );
    let core_wat = wasmprinter::print_bytes(core_wasm).context("Printing core Wasm to WAT")?;
    let core_body = core_wat
        .strip_prefix("(module")
        .and_then(|b| b.trim_end().strip_suffix(')'))
        .context("Expected a valid core Wasm module")?;

    let (clock_import, clock_wire) = if contract.uses_p3_clocks {
        (
            P3_CLOCK_ADAPTER,
            r#"      (export "wait-for" (func $delay "wait-for"))
      (export "waitFor" (func $delay "wait-for"))"#,
        )
    } else {
        ("", "")
    };

    let mut host_imports = String::new();
    let mut host_wires = String::new();

    for (_name, intrinsic) in &contract.intrinsics {
        match intrinsic {
            TypedIntrinsic::HostDouble => {
                host_imports.push_str(
                    r#"  (import "host-double" (func $host-double async (param "value" f64) (result f64)))
  (core func $host-double (canon lower (func $host-double)))
"#,
                );
                host_wires.push_str(
                    r#"      (export "hostDouble" (func $host-double))
      (export "double" (func $host-double))
"#,
                );
            }
            TypedIntrinsic::WaitFor => {
                // Handled via clock_wire above
            }
            TypedIntrinsic::ByteAt => {
                // Synchronous host export wire if needed
            }
            _ => {}
        }
    }

    let component_wat = format!(
        r#"(component
{clock_import}
{host_imports}
  (core module $guest {core_body})
  (core instance $guest (instantiate $guest
    (with "host" (instance
{host_wires}
      {clock_wire}))))
  (func (export "run") async (param "input" f64) (result f64)
    (canon lift (core func $guest "run"))))"#
    );
    let component_bytes =
        wat::parse_str(&component_wat).context("Encoding component WAT to binary")?;

    Ok((component_wat, component_bytes))
}
