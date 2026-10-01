//! WIT world export introspection and function signature resolution.

use std::path::Path;

use anyhow::{Context, Result};
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem, WorldKey};

use crate::component::wit::resolve_wit;

/// Supported Canonical ABI types for task inputs and outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiType {
    String,
    JsonRecord,
    I32,
    I64,
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
    pub params: Vec<(String, AbiType)>,
    pub result: AbiType,
}

/// Discovered WIT export metadata for a selected world.
#[derive(Debug, Clone)]
pub struct WitWorldExports {
    pub world_name: String,
    pub has_cli_command: bool,
    pub functions: Vec<ExportedWitFunction>,
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

    for (key, item) in &world.exports {
        match item {
            WorldItem::Function(func) => {
                let kebab_name = func.name.clone();
                let params = func
                    .params
                    .iter()
                    .map(|param| (param.name.clone(), lower_wit_type(resolve, &param.ty)))
                    .collect();
                let result = match &func.result {
                    Some(ty) => lower_wit_type(resolve, ty),
                    None => AbiType::Unit,
                };
                functions.push(ExportedWitFunction {
                    name: kebab_name.clone(),
                    kebab_name,
                    params,
                    result,
                });
            }
            WorldItem::Interface { id, .. } => {
                let iface = &resolve.interfaces[*id];
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
                        let kebab_name = iface_func.name.clone();
                        let params = iface_func
                            .params
                            .iter()
                            .map(|param| (param.name.clone(), lower_wit_type(resolve, &param.ty)))
                            .collect();
                        let result = match &iface_func.result {
                            Some(ty) => lower_wit_type(resolve, ty),
                            None => AbiType::Unit,
                        };
                        functions.push(ExportedWitFunction {
                            name: kebab_name.clone(),
                            kebab_name,
                            params,
                            result,
                        });
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
    })
}

fn lower_wit_type(resolve: &Resolve, ty: &Type) -> AbiType {
    match ty {
        Type::String => AbiType::String,
        Type::Bool => AbiType::Bool,
        Type::U8 | Type::U16 | Type::U32 | Type::S8 | Type::S16 | Type::S32 => AbiType::I32,
        Type::U64 | Type::S64 => AbiType::I64,
        Type::F32 | Type::F64 => AbiType::F64,
        Type::Id(id) => match &resolve.types[*id].kind {
            TypeDefKind::Record(_) => AbiType::JsonRecord,
            TypeDefKind::Result(_) => AbiType::ResultString,
            TypeDefKind::Type(inner) => lower_wit_type(resolve, inner),
            _ => AbiType::String,
        },
        _ => AbiType::String,
    }
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
}
