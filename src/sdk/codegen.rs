//! WIT to TypeScript declaration (.d.ts) code generator.
//!
//! Generates idiomatic, strongly-typed TypeScript definitions for WIT worlds,
//! exported functions, records, variants, enums, and imported interfaces.

use super::names::{TsNames, collect_types, to_camel_case, to_pascal_case, visit_type_children};
use anyhow::Result;
use std::collections::HashSet;
use wit_parser::{
    Function, Resolve, Result_ as WitResult, Type, TypeDefKind, World, WorldItem, WorldKey,
};

/// Formats a WIT Type into its corresponding TypeScript representation.
fn wit_type_to_ts(names: &TsNames<'_>, ty: &Type) -> String {
    match ty {
        Type::Bool => "boolean".to_string(),
        Type::U8
        | Type::U16
        | Type::U32
        | Type::S8
        | Type::S16
        | Type::S32
        | Type::F32
        | Type::F64 => "number".to_string(),
        Type::U64 | Type::S64 => "bigint".to_string(),
        Type::Char | Type::String => "string".to_string(),
        Type::ErrorContext => "Error | unknown".to_string(),
        Type::Id(id) => {
            let type_def = &names.resolve.types[*id];
            if let Some(name) = names.name(*id) {
                name
            } else {
                type_kind_to_ts(names, &type_def.kind)
            }
        }
    }
}

fn type_kind_to_ts(names: &TsNames<'_>, kind: &TypeDefKind) -> String {
    match kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id)) => {
            wit_type_to_ts(names, &Type::Id(*id))
        }
        TypeDefKind::Stream(Some(Type::U8)) => "ReadableStream<Uint8Array>".into(),
        TypeDefKind::List(elem) => {
            if matches!(elem, Type::U8) {
                "Uint8Array".to_string()
            } else {
                format!("Array<{}>", wit_type_to_ts(names, elem))
            }
        }
        TypeDefKind::Option(inner) => {
            format!("{} | null", wit_type_to_ts(names, inner))
        }
        TypeDefKind::Result(WitResult { ok, err }) => {
            let ok = ok
                .as_ref()
                .map(|ty| format!("; value: {}", wit_type_to_ts(names, ty)))
                .unwrap_or_default();
            let err = err
                .as_ref()
                .map(|ty| format!("; error: {}", wit_type_to_ts(names, ty)))
                .unwrap_or_default();
            format!("{{ ok: true{ok} }} | {{ ok: false{err} }}")
        }
        TypeDefKind::Tuple(tuple) => {
            let items: Vec<String> = tuple
                .types
                .iter()
                .map(|t| wit_type_to_ts(names, t))
                .collect();
            format!("[{}]", items.join(", "))
        }
        _ => "unknown".to_string(),
    }
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
    let ordinary = wit_type_to_ts(names, ty);
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
        .and_then(|ty| crate::abi::result_type(names.resolve, ty))
    {
        types.push(
            result
                .ok
                .map_or_else(|| "void".into(), |ty| outbound_ts(names, &ty)),
        );
    }
    types
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
                signature.push_str(&format!("({result}) | Promise<{result}>"));
            } else if function.kind.is_async() {
                signature.push_str(&format!("Promise<{result}>"));
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
        Some(ty) => wit_type_to_ts(names, ty),
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
pub(crate) fn generate_world_declarations(resolve: &Resolve, world: &World) -> Result<String> {
    crate::abi::export_names::validate_implementation_names(resolve, world)?;
    let names = &TsNames::new(resolve, world);
    let mut out = String::new();

    out.push_str("/**\n * Auto-generated TypeScript definitions for WIT world `");
    out.push_str(&world.name);
    out.push_str("`.\n * Generated by Perry-WIT SDK compiler.\n */\n\n");

    out.push_str(SUPPORT_TYPES);
    out.push_str(&world_type_declarations(names));

    // 2. Emit exported functions
    out.push_str("/* =========================================================================\n");
    out.push_str(
        " * Exported Component Functions (Implement and export in your TypeScript file)\n",
    );
    out.push_str(
        " * ========================================================================= */\n\n",
    );

    for (key, item) in &world.exports {
        match item {
            WorldItem::Function(func) => {
                let export_name = match key {
                    WorldKey::Name(name) => name,
                    WorldKey::Interface(_) => &func.name,
                };
                let ts_func_name = to_camel_case(export_name);
                let ret_ty = export_result_types(names, func).join(" | ");
                let ret_ty = if func.kind.is_async() {
                    format!("Promise<{ret_ty}>")
                } else {
                    ret_ty
                };

                out.push_str("/**\n * WIT export: `");
                out.push_str(export_name);
                out.push_str("`\n */\nexport declare function ");
                out.push_str(&ts_func_name);
                out.push('(');
                emit_params(&mut out, names, func);
                out.push_str("): ");
                out.push_str(&ret_ty);
                out.push_str(";\n");

                // Also generate a type signature alias
                out.push_str("export type ");
                out.push_str(&to_pascal_case(export_name));
                out.push_str("Fn = ");
                out.push_str(&implementation_signature(names, func, false));
                out.push_str(";\n\n");
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

                out.push_str("export namespace ");
                out.push_str(&iface_name);
                out.push_str(" {\n");
                for (_, func) in &iface.functions {
                    let ts_name = to_camel_case(&func.name);
                    let ret_ty = export_result_types(names, func).join(" | ");
                    let ret_ty = if func.kind.is_async() {
                        format!("Promise<{ret_ty}>")
                    } else {
                        ret_ty
                    };
                    out.push_str("  export function ");
                    out.push_str(&ts_name);
                    out.push('(');
                    emit_params(&mut out, names, func);
                    out.push_str("): ");
                    out.push_str(&ret_ty);
                    out.push_str(";\n");
                }
                out.push_str("}\n\n");
            }
            _ => {}
        }
    }

    out.push_str(
        "export interface ComponentImplementation {\n  readonly [name: string]: unknown;\n",
    );
    for entry in crate::abi::world_functions(resolve, world.exports.iter()) {
        out.push_str("  ");
        out.push_str(&entry.implementation_name(resolve, world));
        if crate::abi::export_names::core_export_name(resolve, entry.key, entry.function)
            == "wasi:cli/run@0.3.0#run"
        {
            out.push('?');
        }
        out.push_str(": ");
        out.push_str(&implementation_signature(names, entry.function, true));
        out.push_str(";\n");
    }
    out.push_str("}\n");
    Ok(out)
}

/// Ambient modules expose typed host functions and interface-local type names.
pub(crate) fn generate_import_declarations(resolve: &Resolve, world: &World) -> String {
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
        out.push_str(&format!("declare module {module} {{\n"));
        for name in &declarations {
            out.push_str(&format!("  type {name} = import(\"./world\").{name};\n"));
        }
        for (name, id) in &interface.types {
            let local = to_pascal_case(name);
            let target = wit_type_to_ts(names, &Type::Id(*id));
            if has_bytes(names.resolve, &Type::Id(*id)) {
                out.push_str(&format!(
                    "  export type {local}Input = import(\"./world\").WitInput<{target}>;\n"
                ));
            }
            if local == target {
                out.push_str(&format!("  export {{ type {target} }};\n"));
            } else {
                out.push_str(&format!("  export type {local} = {target};\n"));
            }
        }
        for (name, id) in &interface.types {
            if !matches!(names.resolve.types[*id].kind, TypeDefKind::Resource) {
                continue;
            }
            let resource = wit_type_to_ts(names, &Type::Id(*id));
            let suffix = to_pascal_case(name);
            out.push_str(&format!("  /** Dispose an owned resource. Aliases become invalid. */\n  export function drop{suffix}(value: {resource}): void;\n"));
            if !imported {
                out.push_str(&format!("  export function new{suffix}(representation: number): {resource};\n  export function {local}Rep(value: {resource}): number;\n", local=to_camel_case(name)));
            }
        }
        if imported {
            for function in interface.functions.values() {
                out.push_str(&format!(
                    "  export function {}(",
                    to_camel_case(&function.name)
                ));
                emit_function_params(&mut out, names, function, outbound_ts);
                out.push_str(&format!("): {};\n", wit_result_to_ts(names, function)));
            }
        }
        out.push_str("}\n\n");
    }
    out
}

fn emit_params(out: &mut String, names: &TsNames<'_>, function: &Function) {
    emit_function_params(out, names, function, wit_type_to_ts);
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
        let name = perry_parser::swc_ecma_ast::Ident::verify_symbol(&name)
            .err()
            .unwrap_or(name);
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&type_to_ts(names, &param.ty));
    }
}

fn emit_type_def(names: &TsNames<'_>, type_id: wit_parser::TypeId, out: &mut String) {
    let Some(type_name) = names.name(type_id) else {
        return;
    };
    let td = &names.resolve.types[type_id];
    if has_bytes(names.resolve, &Type::Id(type_id)) {
        out.push_str(&format!(
            "export type {type_name}Input = WitInput<{type_name}>;\n"
        ));
    }
    match &td.kind {
        TypeDefKind::Resource => {
            out.push_str("export declare class ");
            out.push_str(&type_name);
            out.push_str(" { private constructor(); readonly [__witResourceBrand]: true; private readonly __witResource: never; }\n\n");
        }
        TypeDefKind::Record(record) => {
            out.push_str("export interface ");
            out.push_str(&type_name);
            out.push_str(" {\n");
            for field in &record.fields {
                let fname = to_camel_case(&field.name);
                let fty = wit_type_to_ts(names, &field.ty);
                out.push_str("  ");
                out.push_str(&fname);
                out.push_str(": ");
                out.push_str(&fty);
                out.push_str(";\n");
            }
            out.push_str("}\n\n");
        }
        TypeDefKind::Enum(en) => {
            let cases: Vec<String> = en.cases.iter().map(|c| format!("\"{}\"", c.name)).collect();
            out.push_str("export type ");
            out.push_str(&type_name);
            out.push_str(" = ");
            out.push_str(&cases.join(" | "));
            out.push_str(";\n\n");
        }
        TypeDefKind::Variant(var) => {
            let cases: Vec<String> = var
                .cases
                .iter()
                .map(|c| {
                    if let Some(payload) = &c.ty {
                        let ty_str = wit_type_to_ts(names, payload);
                        format!("{{ tag: \"{}\"; val: {ty_str} }}", c.name)
                    } else {
                        format!("{{ tag: \"{}\" }}", c.name)
                    }
                })
                .collect();
            out.push_str("export type ");
            out.push_str(&type_name);
            out.push_str(" =\n  | ");
            out.push_str(&cases.join("\n  | "));
            out.push_str(";\n\n");
        }
        TypeDefKind::Flags(flags) => {
            out.push_str("export interface ");
            out.push_str(&type_name);
            out.push_str(" {\n");
            for f in &flags.flags {
                out.push_str("  ");
                out.push_str(&to_camel_case(&f.name));
                out.push_str("?: boolean;\n");
            }
            out.push_str("}\n\n");
        }
        TypeDefKind::Type(alias) => {
            let alias_ty = wit_type_to_ts(names, alias);
            out.push_str("export type ");
            out.push_str(&type_name);
            out.push_str(" = ");
            out.push_str(&alias_ty);
            out.push_str(";\n\n");
        }
        TypeDefKind::List(_)
        | TypeDefKind::Option(_)
        | TypeDefKind::Tuple(_)
        | TypeDefKind::Result(_) => {
            out.push_str("export type ");
            out.push_str(&type_name);
            out.push_str(" = ");
            out.push_str(&type_kind_to_ts(names, &td.kind));
            out.push_str(";\n\n");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_declarations_with_complex_types() {
        let wit_code = r#"
package test:complex;

interface types {
    record payload {
        id: u32,
        title: string,
        tags: list<string>,
        meta: option<string>,
    }

    enum status {
        pending,
        active,
        failed,
    }

    variant output-result {
        success(string),
        failure(u32),
    }
}

world complex-processor {
    use types.{payload, status, output-result};
    export process: func(data: payload, stat: status) -> output-result;
}
"#;
        let (resolve, package) =
            crate::component::wit::resolve_source("world.wit", wit_code).unwrap();
        let world = resolve
            .select_world(&[package], Some("complex-processor"))
            .unwrap();
        let dts = generate_world_declarations(&resolve, &resolve.worlds[world]).unwrap();
        assert!(dts.contains("export interface Payload {"));
        assert!(dts.contains("id: number;"));
        assert!(dts.contains("title: string;"));
        assert!(dts.contains("tags: Array<string>;"));
        assert!(dts.contains("meta: string | null;"));
        assert!(dts.contains("export type Status = \"pending\" | \"active\" | \"failed\";"));
        assert!(dts.contains("export type OutputResult ="));
        assert!(dts.contains("tag: \"success\"; val: string"));
        assert!(dts.contains("tag: \"failure\"; val: number"));
        assert!(dts.contains(
            "export declare function process(data: Payload, stat: Status): OutputResult;"
        ));
    }

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
        let (resolve, package) =
            crate::component::wit::resolve_source("world.wit", wit_code).unwrap();
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
