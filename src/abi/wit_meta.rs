//! WIT world export introspection and function signature resolution.

use std::path::Path;

use anyhow::{Context, Result, bail};
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem, WorldKey};

use crate::component::wit::resolve_wit;

/// Supported Canonical ABI types for task inputs and outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiType {
    String,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Bool,
    ResultString,
    Unit,
}

/// Metadata describing an exported WIT function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedWitFunction {
    pub name: String,
    pub kebab_name: String,
    pub core_name: String,
    pub implementation_name: String,
    pub params: Vec<(String, AbiType)>,
    pub result: AbiType,
}

/// Discovered WIT export metadata for a selected world.
#[derive(Debug, Clone)]
pub struct WitWorldExports {
    pub world_name: String,
    pub has_cli_command: bool,
    pub functions: Vec<ExportedWitFunction>,
    pub incoming_handler: Option<HttpHandlerExport>,
}

/// The resource ABI stays in generated Rust bindings; TypeScript receives a buffered Request.
#[derive(Debug, Clone)]
pub struct HttpHandlerExport {
    pub core_name: String,
    pub implementation_name: String,
}

/// Resolves WIT files from `wit_dir` and extracts export definitions for `world_name`.
pub fn extract_world_exports(wit_dir: &Path, world_name: Option<&str>) -> Result<WitWorldExports> {
    let (resolve, pkg_id) = resolve_wit(wit_dir)?;
    let world_id = resolve
        .select_world(&[pkg_id], world_name)
        .with_context(|| format!("selecting world {:?}", world_name))?;

    extract_from_world(&resolve, world_id)
}

/// Extracts export function definitions from an already-resolved `WorldId`.
pub fn extract_from_world(resolve: &Resolve, world_id: WorldId) -> Result<WitWorldExports> {
    let world = &resolve.worlds[world_id];
    let world_name = world.name.clone();

    let mut has_cli_command = false;
    let mut functions = Vec::new();
    let mut incoming_handler = None;

    for (key, item) in &world.exports {
        match item {
            WorldItem::Function(func) => {
                functions.push(lower_function(resolve, func)?);
            }
            WorldItem::Interface { id, .. } => {
                let iface = &resolve.interfaces[*id];
                if super::export_names::is_incoming_handler(resolve, *id) {
                    let function = iface
                        .functions
                        .get("handle")
                        .context("missing HTTP handle function")?;
                    let core_name = super::export_names::core_export_name(resolve, key, function);
                    anyhow::ensure!(
                        core_name == "wasi:http/incoming-handler@0.2.6#handle",
                        "incoming handlers require the unaliased wasi:http/incoming-handler@0.2.6 interface"
                    );
                    incoming_handler = Some(HttpHandlerExport {
                        core_name,
                        implementation_name: super::export_names::interface_implementation_name(
                            resolve, world, key, function,
                        ),
                    });
                    continue;
                }
                let mut is_wasi_cli = false;
                if let Some(pkg_id) = iface.package {
                    let pkg = &resolve.packages[pkg_id];
                    if pkg.name.namespace == "wasi"
                        && pkg.name.name == "cli"
                        && iface.name.as_deref() == Some("run")
                    {
                        has_cli_command = true;
                        is_wasi_cli = true;
                    }
                }
                if let WorldKey::Name(name) = key
                    && (name.contains("cli/run") || name.contains("cli/command"))
                {
                    has_cli_command = true;
                    is_wasi_cli = true;
                }
                if !is_wasi_cli {
                    for (_, iface_func) in &iface.functions {
                        let mut function = lower_function(resolve, iface_func)?;
                        function.core_name =
                            super::export_names::core_export_name(resolve, key, iface_func);
                        function.implementation_name =
                            super::export_names::interface_implementation_name(
                                resolve, world, key, iface_func,
                            );
                        functions.push(function);
                    }
                }
            }
            _ => {}
        }
    }

    // Check world includes for wasi:cli/command
    for include in &world.includes {
        let included_world = &resolve.worlds[include.id];
        if included_world.name.contains("command") || included_world.name.contains("run") {
            has_cli_command = true;
        }
    }

    Ok(WitWorldExports {
        world_name,
        has_cli_command,
        functions,
        incoming_handler,
    })
}

fn lower_function(
    resolve: &Resolve,
    function: &wit_parser::Function,
) -> Result<ExportedWitFunction> {
    let params = function
        .params
        .iter()
        .map(|param| {
            Ok((
                param.name.clone(),
                lower_wit_type(resolve, &param.ty).with_context(|| {
                    format!("parameter '{}' of export '{}'", param.name, function.name)
                })?,
            ))
        })
        .collect::<Result<_>>()?;
    let result = function
        .result
        .as_ref()
        .map(|ty| lower_wit_type(resolve, ty))
        .transpose()
        .with_context(|| format!("result of export '{}'", function.name))?
        .unwrap_or(AbiType::Unit);
    Ok(ExportedWitFunction {
        name: function.name.clone(),
        kebab_name: function.name.clone(),
        core_name: function.name.clone(),
        implementation_name: crate::sdk::codegen::to_camel_case(&function.name),
        params,
        result,
    })
}

fn lower_wit_type(resolve: &Resolve, ty: &Type) -> Result<AbiType> {
    Ok(match ty {
        Type::String => AbiType::String,
        Type::Bool => AbiType::Bool,
        Type::S8 | Type::S16 | Type::S32 => AbiType::I32,
        Type::U8 | Type::U16 | Type::U32 => AbiType::U32,
        Type::S64 => AbiType::I64,
        Type::U64 => AbiType::U64,
        Type::F32 => AbiType::F32,
        Type::F64 => AbiType::F64,
        Type::Id(id) => match &resolve.types[*id].kind {
            TypeDefKind::Record(_) => bail!(
                "WIT record '{}' is unsupported: canonical record marshalling is not implemented",
                resolve.types[*id].name.as_deref().unwrap_or("<anonymous>")
            ),
            TypeDefKind::Result(result) => {
                if let (Some(ok), Some(err)) = (&result.ok, &result.err)
                    && lower_wit_type(resolve, ok)? == AbiType::String
                    && lower_wit_type(resolve, err)? == AbiType::String
                {
                    AbiType::ResultString
                } else {
                    bail!(
                        "unsupported WIT result layout: only result<string, string> is implemented"
                    )
                }
            }
            TypeDefKind::Type(inner) => return lower_wit_type(resolve, inner),
            kind => bail!("unsupported WIT ABI type: {}", kind.as_str()),
        },
        other => bail!("unsupported WIT ABI type: {other:?}"),
    })
}

/// Converts a camelCase or snake_case string identifier into kebab-case.
pub fn to_kebab_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 && !out.ends_with('-') {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '_' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    out
}

/// Tests whether a TypeScript function name matches a WIT exported function name.
pub fn matches_export_name(ts_name: &str, wit_name: &str) -> bool {
    ts_name == wit_name
        || to_kebab_case(ts_name) == wit_name
        || ts_name.replace('_', "-") == wit_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_kebab_case() {
        assert_eq!(to_kebab_case("runTask"), "run-task");
        assert_eq!(to_kebab_case("mergeDocs"), "merge-docs");
        assert_eq!(to_kebab_case("run_task"), "run-task");
        assert_eq!(to_kebab_case("simple"), "simple");
    }

    #[test]
    fn test_matches_export_name() {
        assert!(matches_export_name("runTask", "run-task"));
        assert!(matches_export_name("run_task", "run-task"));
        assert!(matches_export_name("run-task", "run-task"));
        assert!(matches_export_name("runTask", "runTask"));
        assert!(!matches_export_name("other", "run-task"));
    }

    #[test]
    fn test_extract_world_exports_merge_docs() {
        let exports = extract_world_exports(Path::new("wit"), Some("merge-docs")).unwrap();
        assert_eq!(exports.world_name, "merge-docs");
        assert!(exports.has_cli_command);
        assert!(exports.functions.is_empty());
    }

    #[test]
    fn test_extract_world_exports_job_runner() {
        let exports = extract_world_exports(Path::new("wit"), Some("task-runner")).unwrap();
        assert_eq!(exports.world_name, "task-runner");
        assert!(!exports.has_cli_command);
        assert_eq!(exports.functions.len(), 1);
        let func = &exports.functions[0];
        assert_eq!(func.name, "run-task");
        assert_eq!(func.kebab_name, "run-task");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].1, AbiType::String);
        assert_eq!(func.result, AbiType::String);
    }
}
