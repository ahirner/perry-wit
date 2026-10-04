//! WebAssembly Component Model framing for WAFFLE core modules, supporting WASI 0.3 (P3).

pub(crate) mod forward;

use std::collections::BTreeSet;

use crate::waffle_backend::capabilities::{CapabilityImplementation, LowerCapability};
use crate::waffle_backend::registry::canonical_param_types;
use crate::waffle_backend::resolve::{ResolvedContract, TypedIntrinsic};
use anyhow::{Context, Result, bail, ensure};
use perry_hir::types::Type as HirType;

/// Frame core Wasm bytecode into a Component Model component.
pub(crate) fn frame_component(
    core_wasm: &[u8],
    contract: &ResolvedContract,
    has_post_return: bool,
) -> Result<(String, Vec<u8>)> {
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
                if let CapabilityImplementation::Standalone {
                    adapter,
                    core_function,
                } = plan.implementation
                {
                    if emitted_operations.insert(operation) {
                        host_imports.push_str(adapter);
                    }
                    host_wires.push_str(&format!("      (export {name:?} {core_function})\n"));
                }
            }
            TypedIntrinsic::ByteAt
            | TypedIntrinsic::ReadChunk
            | TypedIntrinsic::ReadInto
            | TypedIntrinsic::DecoderNew => {}
            TypedIntrinsic::Custom { .. } => bail!(
                "Intrinsic '{}' is unsupported in components until its import adapter is implemented",
                intrinsic.name()
            ),
        }
    }

    let output_operations = contract.output_operations();
    host_imports.push_str(&super::streams::output::declare_adapters(
        &output_operations,
    )?);
    let output_adapters = super::streams::output::bind_adapters(&output_operations)?;
    let output_imports = if output_operations.is_empty() {
        ""
    } else {
        r#"(with "output" (instance $output-forward))"#
    };

    if let Some(plan) = &contract.promises {
        let wat = super::promises::component::frame(
            core_body,
            &host_imports,
            &host_wires,
            contract,
            plan,
            output_imports,
            &output_adapters,
        )?;
        let bytes = wat::parse_str(&wat).context("Encoding stored-Promise component")?;
        return Ok((wat, bytes));
    }

    let needs_allocation = contract.entry_params.iter().any(requires_allocation)
        || requires_allocation(contract.entry_result_type());
    let memory_option = if needs_allocation {
        r#" (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))"#
    } else if contract.entry_returns_wit_result() {
        r#" (memory (core memory $guest "memory"))"#
    } else {
        ""
    };
    let post_return_option = if has_post_return {
        r#" (post-return (core func $guest "cabi_post_run"))"#
    } else {
        ""
    };

    let (stream_imports, stream_adapters) = if contract.has_stream_input() {
        let functions = super::streams::forward_functions();
        host_imports.push_str(&forward::declare("streams", &functions)?);
        let adapters = format!(
            r#"
  (type $bytes (stream u8))
  (core func $stream-read (canon stream.read $bytes (memory (core memory $guest "memory"))))
  (core func $stream-drop (canon stream.drop-readable $bytes))
{}"#,
            forward::bind("streams", &functions)?
        );
        (r#"(with "streams" (instance $streams-forward))"#, adapters)
    } else {
        ("", String::new())
    };

    let component_wat = format!(
        r#"(component
{host_imports}
  (core module $guest {core_body})
  (core instance $guest (instantiate $guest
    {stream_imports}
    {output_imports}
    (with "host" (instance
{host_wires}))))
{stream_adapters}
{output_adapters}
  (func (export "run") async {entry_signature}
    (canon lift (core func $guest "run"){memory_option}{post_return_option})))"#
    );
    let component_bytes =
        wat::parse_str(&component_wat).context("Encoding component WAT to binary")?;

    Ok((component_wat, component_bytes))
}

/// Describe the entry's primitive canonical ABI, retaining booleans as component booleans.
pub(crate) fn entry_signature(contract: &ResolvedContract) -> Result<String> {
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
pub(crate) fn component_value_type(ty: &HirType) -> Result<String> {
    match ty {
        HirType::Number | HirType::Any => Ok("f64".into()),
        HirType::Boolean => Ok("bool".into()),
        HirType::String => Ok("string".into()),
        ty if super::bytes::is_byte_view(ty) => Ok("(list u8)".into()),
        HirType::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => {
            let ok = component_value_type(&type_args[0])?;
            let err = component_value_type(&type_args[1])?;
            Ok(format!("(result {ok} (error {err}))"))
        }
        HirType::Named(name) if name == "ByteStream" => Ok("(stream u8)".into()),
        _ => bail!("Unsupported component entry type: {ty:?}"),
    }
}

fn requires_allocation(ty: &HirType) -> bool {
    match ty {
        HirType::String => true,
        HirType::Generic { base, type_args } if base == "Result" => {
            type_args.iter().any(requires_allocation)
        }
        ty => super::bytes::is_byte_view(ty),
    }
}
