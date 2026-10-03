//! WebAssembly Component Model framing for WAFFLE core modules, supporting WASI 0.3 (P3).

use std::collections::BTreeSet;

use crate::waffle_backend::capabilities::LowerCapability;
use crate::waffle_backend::registry::canonical_param_types;
use crate::waffle_backend::resolve::{ResolvedContract, ResolvedInputKind, TypedIntrinsic};
use anyhow::{Context, Result, bail, ensure};
use perry_hir::types::Type as HirType;

/// Frame core Wasm bytecode into a Component Model component.
pub(crate) fn frame_component(
    core_wasm: &[u8],
    contract: &ResolvedContract,
) -> Result<(String, Vec<u8>)> {
    ensure!(
        contract.input_kind != ResolvedInputKind::ByteStream,
        "ByteStream componentization is unsupported until its canonical ABI adapter is implemented"
    );
    let entry_signature = entry_signature(contract)?;
    let core_wat = wasmprinter::print_bytes(core_wasm).context("Printing core Wasm to WAT")?;
    let core_body = core_wat
        .strip_prefix("(module")
        .and_then(|b| b.trim_end().strip_suffix(')'))
        .context("Expected a valid core Wasm module")?;

    let mut host_imports = String::new();
    let mut host_wires = String::new();
    let mut emitted_operations = BTreeSet::new();

    for (name, intrinsic) in &contract.intrinsics {
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
            TypedIntrinsic::Capability(operation) => {
                let plan = operation.lower();
                if emitted_operations.insert(operation) {
                    host_imports.push_str(plan.adapter);
                }
                host_wires.push_str(&format!("      (export {name:?} {})\n", plan.core_function));
            }
            TypedIntrinsic::ByteAt
            | TypedIntrinsic::ReadChunk
            | TypedIntrinsic::StreamDrop
            | TypedIntrinsic::StreamReset
            | TypedIntrinsic::Custom { .. } => bail!(
                "Intrinsic '{}' is unsupported in components until its import adapter is implemented",
                intrinsic.name()
            ),
        }
    }

    let has_string_or_realloc = contract
        .entry_params
        .iter()
        .any(|ty| matches!(ty, HirType::String))
        || matches!(contract.entry_result_type(), HirType::String)
        || if let HirType::Generic { base, type_args } = contract.entry_result_type() {
            base == "Result" && type_args.iter().any(|ty| matches!(ty, HirType::String))
        } else {
            false
        };
    let memory_option = if has_string_or_realloc {
        r#" (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))"#
    } else if contract.entry_returns_wit_result() {
        r#" (memory (core memory $guest "memory"))"#
    } else {
        ""
    };

    let component_wat = format!(
        r#"(component
{host_imports}
  (core module $guest {core_body})
  (core instance $guest (instantiate $guest
    (with "host" (instance
{host_wires}))))
  (func (export "run") async {entry_signature}
    (canon lift (core func $guest "run"){memory_option})))"#
    );
    let component_bytes =
        wat::parse_str(&component_wat).context("Encoding component WAT to binary")?;

    Ok((component_wat, component_bytes))
}

/// Describe the entry's primitive canonical ABI, retaining booleans as component booleans.
fn entry_signature(contract: &ResolvedContract) -> Result<String> {
    ensure!(
        canonical_param_types(&contract.entry_params)?.len() <= 16,
        "Entry functions with more than 16 flattened parameters require an unsupported canonical ABI adapter"
    );
    let mut signature = String::new();
    for (index, ty) in contract.entry_params.iter().enumerate() {
        let ty = component_value_type(ty)?;
        signature.push_str(&format!("(param \"arg-{index}\" {ty}) "));
    }
    let return_type = contract.entry_result_type();
    if !matches!(return_type, HirType::Void) {
        let ty = component_value_type(return_type)?;
        signature.push_str(&format!("(result {ty})"));
    }
    Ok(signature)
}

/// Map supported entry values without confusing core handles with component streams.
fn component_value_type(ty: &HirType) -> Result<String> {
    match ty {
        HirType::Number | HirType::Any => Ok("f64".into()),
        HirType::Boolean => Ok("bool".into()),
        HirType::String => Ok("string".into()),
        HirType::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => {
            let ok = component_value_type(&type_args[0])?;
            let err = component_value_type(&type_args[1])?;
            Ok(format!("(result {ok} (error {err}))"))
        }
        HirType::Named(name) if name == "ByteStream" => bail!(
            "ByteStream componentization is unsupported until its canonical ABI adapter is implemented"
        ),
        _ => bail!("Unsupported component entry type: {ty:?}"),
    }
}
