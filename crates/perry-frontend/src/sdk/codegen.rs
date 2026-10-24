//! WIT to TypeScript declaration (.d.ts) code generator.
//!
//! Generates idiomatic, strongly-typed TypeScript definitions for WIT worlds,
//! exported functions, records, variants, enums, and imported interfaces.

use anyhow::Result;
use perry_contract::names::{
    TsNames, collect_types, to_camel_case, to_pascal_case, visit_type_children,
};
use perry_contract::types::{Contract, Input, convert};
use std::{collections::HashSet, fmt::Write as _};
use wit_parser::{Function, Resolve, Type, TypeDefKind, World, WorldItem, WorldKey};

fn type_to_ts(names: &TsNames<'_>, input: Input) -> String {
    convert(
        names.resolve,
        input,
        &|id| names.name(id),
        &|contract| match contract {
            Contract::Scalar(ty) => scalar_type(ty).into(),
            Contract::Resource { id, .. } => names.name(id).unwrap_or_else(|| "unknown".into()),
            Contract::Bytes => "Uint8Array".into(),
            Contract::ByteStream => "ReadableStream<Uint8Array>".into(),
            Contract::List(ty) => format!("Array<{ty}>"),
            Contract::Option(ty) => format!("{ty} | null"),
            Contract::Result { ok, err } => {
                let ok = ok.map(|ty| format!("; value: {ty}")).unwrap_or_default();
                let err = err.map(|ty| format!("; error: {ty}")).unwrap_or_default();
                format!("{{ ok: true{ok} }} | {{ ok: false{err} }}")
            }
            Contract::Tuple(items) => {
                format!("[{}]", items.collect::<Vec<_>>().join(", "))
            }
            Contract::Record(fields) => {
                let mut out = String::from("{\n");
                for (name, ty) in fields {
                    let _ = writeln!(out, "  {name}: {ty};");
                }
                out.push('}');
                out
            }
            Contract::Enum(cases) => cases
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(" | "),
            Contract::Variant(cases) => {
                let mut out = String::new();
                for (name, payload) in cases {
                    let _ = write!(out, "\n  | {{ tag: \"{name}\"");
                    if let Some(ty) = payload {
                        let _ = write!(out, "; val: {ty}");
                    }
                    out.push_str(" }");
                }
                out
            }
            Contract::Unsupported(_) => "unknown".into(),
        },
    )
}

fn has_bytes(resolve: &Resolve, ty: &Type) -> bool {
    let Type::Id(id) = ty else { return false };
    if matches!(resolve.types[*id].kind, TypeDefKind::List(Type::U8)) {
        return true;
    }
    let mut found = false;
    visit_type_children(&resolve.types[*id].kind, |ty| {
        found = found || has_bytes(resolve, ty);
    });
    found
}
fn outbound_ts(names: &TsNames<'_>, ty: &Type) -> String {
    let ordinary = type_to_ts(names, Input::Reference(*ty));
    if has_bytes(names.resolve, ty) {
        format!("import(\"./world\").WitInput<{ordinary}>")
    } else {
        ordinary
    }
}

/// Result exports accept either explicit tagged results or completion payloads.
fn export_result_types(names: &TsNames<'_>, function: &Function) -> Vec<String> {
    let mut types = vec![
        function
            .result
            .map_or_else(|| "void".into(), |ty| outbound_ts(names, &ty)),
    ];
    if let Some(result) = function
        .result
        .and_then(|ty| perry_contract::result_type(names.resolve, ty))
    {
        types.push(
            result
                .ok
                .map_or_else(|| "void".into(), |ty| outbound_ts(names, &ty)),
        );
    }
    types
}

fn export_return_ts(names: &TsNames<'_>, func: &Function) -> String {
    let ret_ty = export_result_types(names, func).join(" | ");
    if func.kind.is_async() {
        format!("Promise<{ret_ty}>")
    } else {
        ret_ty
    }
}

fn implementation_signature(
    names: &TsNames<'_>,
    function: &Function,
    allow_promise: bool,
) -> String {
    let mut signatures: Vec<String> = export_result_types(names, function)
        .into_iter()
        .map(|result| {
            let mut signature = String::from("(");
            emit_params(&mut signature, names, function);
            signature.push_str(") => ");
            if allow_promise {
                let _ = write!(signature, "({result}) | Promise<{result}>");
            } else if function.kind.is_async() {
                let _ = write!(signature, "Promise<{result}>");
            } else {
                signature.push_str(&result);
            }
            signature
        })
        .collect();
    if signatures.len() == 1 {
        signatures.swap_remove(0)
    } else {
        format!("({})", signatures.join(") | ("))
    }
}

/// Formats a WIT function return type into TypeScript.
fn wit_result_to_ts(names: &TsNames<'_>, func: &Function) -> String {
    let result = match &func.result {
        None => "void".to_string(),
        Some(ty) => type_to_ts(names, Input::Reference(*ty)),
    };
    if func.kind.is_async() {
        format!("Promise<{result}>")
    } else {
        result
    }
}

const SUPPORT_TYPES: &str = "/** Outbound byte lists accept UTF-8 text; inbound values remain Uint8Array. */\ndeclare const __witResourceBrand: unique symbol;\nexport interface WitResource { readonly [__witResourceBrand]: true; }\nexport type WitInput<T> = T extends WitResource ? T : T extends Uint8Array ? string | Uint8Array : T extends object ? { [K in keyof T]: WitInput<T[K]> } : T;\nexport type u8 = number;\nexport type s8 = number;\nexport type u16 = number;\nexport type s16 = number;\nexport type u32 = number;\nexport type s32 = number;\nexport type u64 = bigint;\nexport type s64 = bigint;\n\n";

fn world_type_declarations(names: &TsNames<'_>) -> String {
    let mut out = String::new();
    for (id, _) in &names.resolve.types {
        if names.types.contains(&id) {
            emit_type_def(names, id, &mut out);
        }
    }
    out
}

/// Generates TypeScript declarations (.d.ts) for a given WIT world.
pub(super) fn generate_world_declarations(resolve: &Resolve, world: &World) -> Result<String> {
    perry_contract::export_names::validate_implementation_names(resolve, world)?;
    let names = &TsNames::new(resolve, world);
    let mut out = String::new();

    let _ = write!(
        out,
        "/**\n * Auto-generated TypeScript definitions for WIT world `{}`.\n * Generated by Perry-WIT SDK compiler.\n */\n\n{SUPPORT_TYPES}{}/* =========================================================================\n * Exported Component Functions (Implement and export in your TypeScript file)\n * ========================================================================= */\n\n",
        world.name,
        world_type_declarations(names),
    );

    for (key, item) in &world.exports {
        match item {
            WorldItem::Function(func) => {
                let export_name = match key {
                    WorldKey::Name(name) => name,
                    WorldKey::Interface(_) => &func.name,
                };
                let ts_func_name = to_camel_case(export_name);
                let ret_ty = export_return_ts(names, func);
                let _ = write!(
                    out,
                    "/**\n * WIT export: `{export_name}`\n */\nexport declare function {ts_func_name}("
                );
                emit_params(&mut out, names, func);
                let _ = writeln!(
                    out,
                    "): {ret_ty};\nexport type {}Fn = {};\n",
                    to_pascal_case(export_name),
                    implementation_signature(names, func, false),
                );
            }
            WorldItem::Interface { id, .. } => {
                let iface = &resolve.interfaces[*id];
                let iface_name = match key {
                    WorldKey::Name(n) => to_pascal_case(n),
                    WorldKey::Interface(_) => iface
                        .name
                        .as_deref()
                        .map(to_pascal_case)
                        .unwrap_or_else(|| "ExportedInterface".to_string()),
                };

                let _ = writeln!(out, "export namespace {iface_name} {{");
                for (_, func) in &iface.functions {
                    let ts_name = to_camel_case(&func.name);
                    let ret_ty = export_return_ts(names, func);
                    let _ = write!(out, "  export function {ts_name}(");
                    emit_params(&mut out, names, func);
                    let _ = writeln!(out, "): {ret_ty};");
                }
                out.push_str("}\n\n");
            }
            _ => {}
        }
    }

    out.push_str(
        "export interface ComponentImplementation {\n  readonly [name: string]: unknown;\n",
    );
    for entry in perry_contract::world_functions(resolve, world.exports.iter()) {
        let opt =
            if perry_contract::export_names::core_export_name(resolve, entry.key, entry.function)
                == "wasi:cli/run@0.3.0#run"
            {
                "?"
            } else {
                ""
            };
        let _ = writeln!(
            out,
            "  {}{opt}: {};",
            entry.implementation_name(resolve, world),
            implementation_signature(names, entry.function, true),
        );
    }
    out.push_str("}\n");
    Ok(out)
}

/// Ambient modules expose typed host functions and interface-local type names.
pub(super) fn generate_import_declarations(resolve: &Resolve, world: &World) -> String {
    let names = &TsNames::new(resolve, world);
    let mut out = String::new();
    let mut emitted = HashSet::new();
    for (imported, (key, item)) in world
        .imports
        .iter()
        .map(|item| (true, item))
        .chain(world.exports.iter().map(|item| (false, item)))
    {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        if !emitted.insert(key) {
            continue;
        }
        let module = serde_json::Value::String(resolve.name_world_key(key));
        let interface = &resolve.interfaces[*id];
        let mut ids = HashSet::new();
        for id in interface.types.values() {
            collect_types(resolve, &Type::Id(*id), &mut ids);
        }
        for function in interface.functions.values() {
            for param in &function.params {
                collect_types(resolve, &param.ty, &mut ids);
            }
            if let Some(ty) = function.result {
                collect_types(resolve, &ty, &mut ids);
            }
        }
        let mut declarations: Vec<_> = ids.into_iter().filter_map(|id| names.name(id)).collect();
        declarations.sort();
        declarations.dedup();
        // A JSON string literal is a valid TypeScript module specifier.
        let _ = writeln!(out, "declare module {module} {{");
        for name in &declarations {
            let _ = writeln!(out, "  type {name} = import(\"./world\").{name};");
        }
        for (name, id) in &interface.types {
            let local = to_pascal_case(name);
            let target = type_to_ts(names, Input::Reference(Type::Id(*id)));
            if has_bytes(names.resolve, &Type::Id(*id)) {
                let _ = writeln!(
                    out,
                    "  export type {local}Input = import(\"./world\").WitInput<{target}>;"
                );
            }
            if local == target {
                let _ = writeln!(out, "  export {{ type {target} }};");
            } else {
                let _ = writeln!(out, "  export type {local} = {target};");
            }
        }
        for (name, id) in &interface.types {
            if !matches!(names.resolve.types[*id].kind, TypeDefKind::Resource) {
                continue;
            }
            let resource = type_to_ts(names, Input::Reference(Type::Id(*id)));
            let suffix = to_pascal_case(name);
            let _ = writeln!(
                out,
                "  /** Dispose an owned resource. Aliases become invalid. */\n  export function drop{suffix}(value: {resource}): void;"
            );
            if !imported {
                let _ = writeln!(
                    out,
                    "  export function new{suffix}(representation: number): {resource};\n  export function {local}Rep(value: {resource}): number;",
                    local = to_camel_case(name)
                );
            }
        }
        if imported {
            for function in interface.functions.values() {
                let _ = write!(out, "  export function {}(", to_camel_case(&function.name));
                emit_function_params(&mut out, names, function, outbound_ts);
                let _ = writeln!(out, "): {};", wit_result_to_ts(names, function));
            }
        }
        out.push_str("}\n\n");
    }
    out
}

fn emit_params(out: &mut String, names: &TsNames<'_>, function: &Function) {
    emit_function_params(out, names, function, |names, ty| {
        type_to_ts(names, Input::Reference(*ty))
    });
}

fn emit_function_params(
    out: &mut String,
    names: &TsNames<'_>,
    function: &Function,
    type_to_ts: fn(&TsNames<'_>, &Type) -> String,
) {
    for (index, param) in function.params.iter().enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        let name = to_camel_case(&param.name);
        let name = if crate::is_identifier(&name) {
            name
        } else {
            format!("_{name}")
        };
        let _ = write!(out, "{name}: {}", type_to_ts(names, &param.ty));
    }
}

fn emit_type_def(names: &TsNames<'_>, type_id: wit_parser::TypeId, out: &mut String) {
    let Some(type_name) = names.name(type_id) else {
        return;
    };
    let td = &names.resolve.types[type_id];
    if has_bytes(names.resolve, &Type::Id(type_id)) {
        let _ = writeln!(out, "export type {type_name}Input = WitInput<{type_name}>;");
    }
    match &td.kind {
        TypeDefKind::Resource => {
            let _ = write!(
                out,
                "export declare class {type_name} {{ private constructor(); readonly [__witResourceBrand]: true; private readonly __witResource: never; }}\n\n"
            );
        }
        TypeDefKind::Record(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Variant(_)
        | TypeDefKind::Type(_)
        | TypeDefKind::List(_)
        | TypeDefKind::Option(_)
        | TypeDefKind::Tuple(_)
        | TypeDefKind::Result(_) => {
            let value = type_to_ts(names, Input::Definition(type_id));
            if matches!(td.kind, TypeDefKind::Record(_)) {
                let _ = write!(out, "export interface {type_name} {value}\n\n");
            } else {
                let _ = write!(out, "export type {type_name} = {value};\n\n");
            }
        }
        TypeDefKind::Flags(flags) => {
            let _ = writeln!(out, "export interface {type_name} {{");
            for f in &flags.flags {
                let _ = writeln!(out, "  {}?: boolean;", to_camel_case(&f.name));
            }
            out.push_str("}\n\n");
        }
        _ => {}
    }
}

fn scalar_type(ty: Type) -> &'static str {
    match ty {
        Type::Bool => "boolean",
        Type::U8
        | Type::S8
        | Type::U16
        | Type::S16
        | Type::U32
        | Type::S32
        | Type::F32
        | Type::F64 => "number",
        Type::U64 | Type::S64 => "bigint",
        Type::Char | Type::String => "string",
        Type::ErrorContext => "Error | unknown",
        Type::Id(_) => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreferenced_sibling_interfaces_do_not_prefix_world_type_names() {
        let wit_code = r#"
package test:multi@0.1.0;

interface alpha {
    record parameters {
        url: string,
    }
    record product {
        name: string,
    }
    fetch: func(params: parameters) -> list<product>;
}

interface beta {
    record parameters {
        token: string,
    }
    record product {
        id: u32,
    }
}

world alpha-worker {
    import alpha;
    use alpha.{parameters, product};
    export run: func(params: parameters) -> list<product>;
}
"#;
        let mut resolve = Resolve::default();
        let package = resolve.push_source("world.wit", wit_code).unwrap();
        let world = resolve
            .select_world(&[package], Some("alpha-worker"))
            .unwrap();
        let world_dts = generate_world_declarations(&resolve, &resolve.worlds[world]).unwrap();
        assert!(world_dts.contains("export interface Parameters {"));
        assert!(world_dts.contains("export interface Product {"));
        assert!(!world_dts.contains("TestMulti010AlphaParameters"));
        assert!(!world_dts.contains("TestMulti010AlphaProduct"));

        let import_dts = generate_import_declarations(&resolve, &resolve.worlds[world]);
        assert!(import_dts.contains("type Parameters = import(\"./world\").Parameters;"));
        assert!(import_dts.contains("export { type Parameters };"));
        assert!(import_dts.contains("export function fetch(params: Parameters): Array<Product>;"));
    }
}
