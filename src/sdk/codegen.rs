//! WIT to TypeScript declaration (.d.ts) code generator.
//!
//! Generates idiomatic, strongly-typed TypeScript definitions for WIT worlds,
//! exported functions, records, variants, enums, and imported interfaces.

use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use wit_parser::{
    Function, Resolve, Result_ as WitResult, Type, TypeDefKind, TypeId, TypeOwner, World,
    WorldItem, WorldKey,
};

/// Converts a kebab-case or snake_case string into camelCase.
pub fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = false;
    for (i, c) in s.chars().enumerate() {
        if !c.is_ascii_alphanumeric() {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else if i == 0 {
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// Converts a kebab-case or snake_case string into PascalCase.
pub fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if !c.is_ascii_alphanumeric() {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn declaration_id(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(inner)) = &resolve.types[id].kind {
        if resolve.types[id].name != resolve.types[*inner].name {
            break;
        }
        id = *inner;
    }
    id
}

fn type_name(resolve: &Resolve, id: TypeId) -> String {
    let id = declaration_id(resolve, id);
    let definition = &resolve.types[id];
    let name = to_pascal_case(definition.name.as_deref().unwrap());
    let collides = resolve.types.iter().any(|(other, ty)| {
        declaration_id(resolve, other) != id
            && ty
                .name
                .as_deref()
                .is_some_and(|other| to_pascal_case(other) == name)
    });
    if !collides {
        return name;
    }
    let owner = match definition.owner {
        TypeOwner::Interface(interface) => resolve
            .id_of(interface)
            .unwrap_or_else(|| format!("interface-{}", interface.index())),
        TypeOwner::World(world) => {
            let world = &resolve.worlds[world];
            world
                .package
                .map(|package| resolve.id_of_name(package, &world.name))
                .unwrap_or_else(|| world.name.clone())
        }
        TypeOwner::None => format!("type-{}", id.index()),
    };
    format!("{}{name}", to_pascal_case(&owner))
}

/// Formats a WIT Type into its corresponding TypeScript representation.
pub fn wit_type_to_ts(resolve: &Resolve, ty: &Type) -> String {
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
            let type_def = &resolve.types[*id];
            if type_def.name.is_some() {
                type_name(resolve, *id)
            } else {
                type_kind_to_ts(resolve, &type_def.kind)
            }
        }
    }
}

fn type_kind_to_ts(resolve: &Resolve, kind: &TypeDefKind) -> String {
    match kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id)) => {
            type_name(resolve, *id)
        }
        TypeDefKind::List(elem) => {
            if matches!(elem, Type::U8) {
                "Uint8Array".to_string()
            } else {
                format!("Array<{}>", wit_type_to_ts(resolve, elem))
            }
        }
        TypeDefKind::Option(inner) => {
            format!("{} | null | undefined", wit_type_to_ts(resolve, inner))
        }
        TypeDefKind::Result(WitResult { ok, err }) => {
            let ok = ok
                .as_ref()
                .map(|ty| format!("; value: {}", wit_type_to_ts(resolve, ty)))
                .unwrap_or_default();
            let err = err
                .as_ref()
                .map(|ty| format!("; error: {}", wit_type_to_ts(resolve, ty)))
                .unwrap_or_default();
            format!("{{ ok: true{ok} }} | {{ ok: false{err} }}")
        }
        TypeDefKind::Tuple(tuple) => {
            let items: Vec<String> = tuple
                .types
                .iter()
                .map(|t| wit_type_to_ts(resolve, t))
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
fn outbound_ts(resolve: &Resolve, ty: &Type) -> String {
    let ordinary = wit_type_to_ts(resolve, ty);
    if has_bytes(resolve, ty) {
        format!("import(\"./world\").WitInput<{ordinary}>")
    } else {
        ordinary
    }
}

/// Formats a WIT function return type into TypeScript.
pub fn wit_result_to_ts(resolve: &Resolve, func: &Function) -> String {
    let result = match &func.result {
        None => "void".to_string(),
        Some(ty) => wit_type_to_ts(resolve, ty),
    };
    if func.kind.is_async() {
        format!("Promise<{result}>")
    } else {
        result
    }
}

/// Generates TypeScript declarations (.d.ts) for a given WIT world.
pub fn generate_world_declarations(resolve: &Resolve, world: &World) -> Result<String> {
    crate::abi::export_names::validate_implementation_names(resolve, world)?;
    let mut out = String::new();

    out.push_str("/**\n * Auto-generated TypeScript definitions for WIT world `");
    out.push_str(&world.name);
    out.push_str("`.\n * Generated by Perry-WIT SDK compiler.\n */\n\n");

    out.push_str("/** Outbound byte lists accept UTF-8 text; inbound values remain Uint8Array. */\ndeclare const __witResourceBrand: unique symbol;\nexport interface WitResource { readonly [__witResourceBrand]: true; }\nexport type WitInput<T> = T extends WitResource ? T : T extends Uint8Array ? string | Uint8Array : T extends object ? { [K in keyof T]: WitInput<T[K]> } : T;\n\n");
    let mut emitted_types = HashSet::new();

    // 1. Collect and emit named types referenced in world or interfaces (imports & exports)
    for (_, item) in world.imports.iter().chain(world.exports.iter()) {
        match item {
            WorldItem::Interface { id, .. } => {
                let iface = &resolve.interfaces[*id];
                for &type_id in iface.types.values() {
                    emit_type_def(resolve, type_id, &mut emitted_types, &mut out);
                }
            }
            WorldItem::Function(func) => {
                for param in &func.params {
                    emit_nested_types(resolve, &param.ty, &mut emitted_types, &mut out);
                }
                if let Some(ret) = &func.result {
                    emit_nested_types(resolve, ret, &mut emitted_types, &mut out);
                }
            }
            WorldItem::Type { id: type_id, .. } => {
                let td = &resolve.types[*type_id];
                if td.name.is_some() {
                    emit_type_def(resolve, *type_id, &mut emitted_types, &mut out);
                }
            }
        }
    }

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
                    WorldKey::Name(name) => name.clone(),
                    WorldKey::Interface(_) => func.name.clone(),
                };
                let ts_func_name = to_camel_case(&export_name);
                let ret_ty = func
                    .result
                    .map_or_else(|| "void".into(), |ty| outbound_ts(resolve, &ty));
                let ret_ty = if func.kind.is_async() {
                    format!("Promise<{ret_ty}>")
                } else {
                    ret_ty
                };

                out.push_str("/**\n * WIT export: `");
                out.push_str(&export_name);
                out.push_str("`\n */\nexport declare function ");
                out.push_str(&ts_func_name);
                out.push('(');
                emit_params(&mut out, resolve, func);
                out.push_str("): ");
                out.push_str(&ret_ty);
                out.push_str(";\n");

                // Also generate a type signature alias
                out.push_str("export type ");
                out.push_str(&to_pascal_case(&export_name));
                out.push_str("Fn = (");
                emit_params(&mut out, resolve, func);
                out.push_str(") => ");
                out.push_str(&ret_ty);
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
                    let ret_ty = func
                        .result
                        .map_or_else(|| "void".into(), |ty| outbound_ts(resolve, &ty));
                    let ret_ty = if func.kind.is_async() {
                        format!("Promise<{ret_ty}>")
                    } else {
                        ret_ty
                    };
                    out.push_str("  export function ");
                    out.push_str(&ts_name);
                    out.push('(');
                    emit_params(&mut out, resolve, func);
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
    for (key, item) in &world.exports {
        let functions = match item {
            WorldItem::Function(function) => vec![(to_camel_case(&function.name), function)],
            WorldItem::Interface { id, .. } => {
                let interface = &resolve.interfaces[*id];
                interface
                    .functions
                    .values()
                    .map(|function| {
                        (
                            crate::abi::export_names::interface_implementation_name(
                                resolve, world, key, function,
                            ),
                            function,
                        )
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        for (name, function) in functions {
            out.push_str("  ");
            out.push_str(&name);
            if crate::abi::export_names::core_export_name(resolve, key, function)
                == "wasi:cli/run@0.3.0#run"
            {
                out.push('?');
            }
            out.push_str(": (");
            emit_params(&mut out, resolve, function);
            out.push_str(") => ");
            let result = function
                .result
                .map_or_else(|| "void".into(), |ty| outbound_ts(resolve, &ty));
            out.push_str(&format!("({result}) | Promise<{result}>"));
            out.push_str(";\n");
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// Ambient modules expose typed host functions and interface-local type names.
pub(crate) fn generate_import_declarations(resolve: &Resolve, world: &World) -> String {
    fn collect(resolve: &Resolve, ty: &Type, ids: &mut HashSet<TypeId>) {
        let Type::Id(id) = ty else {
            return;
        };
        let id = declaration_id(resolve, *id);
        if !ids.insert(id) {
            return;
        }
        if let TypeDefKind::Handle(
            wit_parser::Handle::Own(resource) | wit_parser::Handle::Borrow(resource),
        ) = resolve.types[id].kind
        {
            collect(resolve, &Type::Id(resource), ids);
        }
        visit_type_children(&resolve.types[id].kind, |ty| {
            collect(resolve, ty, ids);
        });
    }
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
        let module = resolve.name_world_key(key);
        if !emitted.insert(module.clone()) {
            continue;
        }
        let interface = &resolve.interfaces[*id];
        let mut ids = HashSet::new();
        for id in interface.types.values() {
            collect(resolve, &Type::Id(*id), &mut ids);
        }
        for function in interface.functions.values() {
            for param in &function.params {
                collect(resolve, &param.ty, &mut ids);
            }
            if let Some(ty) = function.result {
                collect(resolve, &ty, &mut ids);
            }
        }
        let mut names: Vec<_> = ids
            .into_iter()
            .filter(|id| resolve.types[*id].name.is_some())
            .map(|id| type_name(resolve, id))
            .collect();
        names.sort();
        names.dedup();
        out.push_str(&format!(
            "declare module {} {{\n",
            serde_json::to_string(&module).unwrap()
        ));
        for name in &names {
            out.push_str(&format!("  type {name} = import(\"./world\").{name};\n"));
        }
        for (name, id) in &interface.types {
            if has_bytes(resolve, &Type::Id(*id)) {
                let local = to_pascal_case(name);
                let target = type_name(resolve, *id);
                out.push_str(&format!(
                    "  export type {local}Input = import(\"./world\").WitInput<{target}>;\n"
                ));
            }
            let local = to_pascal_case(name);
            let target = type_name(resolve, *id);
            if local == target {
                out.push_str(&format!("  export {{ type {target} }};\n"));
            } else {
                out.push_str(&format!("  export type {local} = {target};\n"));
            }
        }
        for (name, id) in &interface.types {
            if !matches!(resolve.types[*id].kind, TypeDefKind::Resource) {
                continue;
            }
            let resource = type_name(resolve, *id);
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
                emit_outbound_params(&mut out, resolve, function);
                out.push_str(&format!("): {};\n", wit_result_to_ts(resolve, function)));
            }
        }
        out.push_str("}\n\n");
    }
    out
}

fn emit_outbound_params(out: &mut String, resolve: &Resolve, function: &Function) {
    emit_function_params(out, resolve, function, outbound_ts);
}

fn emit_params(out: &mut String, resolve: &Resolve, function: &Function) {
    emit_function_params(out, resolve, function, wit_type_to_ts);
}

fn emit_function_params(
    out: &mut String,
    resolve: &Resolve,
    function: &Function,
    type_to_ts: fn(&Resolve, &Type) -> String,
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
        out.push_str(&type_to_ts(resolve, &param.ty));
    }
}

fn emit_nested_types(
    resolve: &Resolve,
    ty: &Type,
    emitted: &mut HashSet<TypeId>,
    out: &mut String,
) {
    if let Type::Id(id) = ty {
        let td = &resolve.types[*id];
        if let TypeDefKind::Handle(
            wit_parser::Handle::Own(resource) | wit_parser::Handle::Borrow(resource),
        ) = td.kind
        {
            emit_type_def(resolve, resource, emitted, out);
        }
        if td.name.is_some() {
            emit_type_def(resolve, *id, emitted, out);
        } else {
            visit_type_children(&td.kind, |child| {
                emit_nested_types(resolve, child, emitted, out);
            });
        }
    }
}

fn visit_type_children(kind: &TypeDefKind, mut visit: impl FnMut(&Type)) {
    match kind {
        TypeDefKind::List(inner) | TypeDefKind::Option(inner) | TypeDefKind::Type(inner) => {
            visit(inner);
        }
        TypeDefKind::Result(result) => result.ok.iter().chain(result.err.iter()).for_each(visit),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().for_each(visit),
        TypeDefKind::Record(record) => record.fields.iter().map(|field| &field.ty).for_each(visit),
        TypeDefKind::Variant(variant) => variant
            .cases
            .iter()
            .filter_map(|case| case.ty.as_ref())
            .for_each(visit),
        _ => {}
    }
}

fn emit_type_def(
    resolve: &Resolve,
    type_id: wit_parser::TypeId,
    emitted: &mut HashSet<TypeId>,
    out: &mut String,
) {
    let type_id = declaration_id(resolve, type_id);
    if !emitted.insert(type_id) {
        return;
    }
    let type_name = type_name(resolve, type_id);
    let td = &resolve.types[type_id];
    if has_bytes(resolve, &Type::Id(type_id)) {
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
                let fty = wit_type_to_ts(resolve, &field.ty);
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
                        let ty_str = wit_type_to_ts(resolve, payload);
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
            let alias_ty = wit_type_to_ts(resolve, alias);
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
            out.push_str(&type_kind_to_ts(resolve, &td.kind));
            out.push_str(";\n\n");
        }
        _ => {}
    }
    visit_type_children(&td.kind, |child| {
        emit_nested_types(resolve, child, emitted, out);
    });
}

/// Generates declarations for a WIT directory and target world name.
pub fn generate_declarations_from_wit_dir(
    wit_dir: &Path,
    world_name: Option<&str>,
) -> Result<(String, String)> {
    let (resolve, pkg_id) = crate::component::wit::resolve_wit(wit_dir)?;
    let pkg = &resolve.packages[pkg_id];

    let target_world_id = if let Some(wname) = world_name {
        pkg.worlds
            .get(wname)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("World `{wname}` not found in WIT package"))?
    } else if let Some((_, &first_world)) = pkg.worlds.iter().next() {
        first_world
    } else {
        anyhow::bail!("No worlds found in WIT package at {}", wit_dir.display());
    };

    let world = &resolve.worlds[target_world_id];
    let dts = generate_world_declarations(&resolve, world)?;
    Ok((world.name.clone(), dts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_camel_case() {
        assert_eq!(to_camel_case("run-task"), "runTask");
        assert_eq!(to_camel_case("merge_docs"), "mergeDocs");
        assert_eq!(to_camel_case("simple"), "simple");
        assert_eq!(to_camel_case("alreadyCamelCase"), "alreadyCamelCase");
    }

    #[test]
    fn test_to_pascal_case() {
        assert_eq!(to_pascal_case("merge-task"), "MergeTask");
        assert_eq!(to_pascal_case("job_runner"), "JobRunner");
        assert_eq!(to_pascal_case("world"), "World");
    }

    #[test]
    fn test_generate_declarations_for_merge_task_world() {
        let (world_name, dts) =
            generate_declarations_from_wit_dir(Path::new("wit"), Some("merge-task"))
                .expect("Failed to generate declarations");

        assert_eq!(world_name, "merge-task");
        assert!(dts.contains("export declare function runTask(input: string): string;"));
        assert!(dts.contains("export declare function mergeTask(input: string): string;"));
        assert!(dts.contains("export type RunTaskFn = (input: string) => string;"));
    }

    #[test]
    fn test_generate_declarations_with_complex_types() {
        let tmp = std::env::temp_dir().join("perry_wit_test_types_wit");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

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
        std::fs::write(tmp.join("world.wit"), wit_code).unwrap();

        let (world_name, dts) = generate_declarations_from_wit_dir(&tmp, Some("complex-processor"))
            .expect("Failed to generate declarations for complex types");

        assert_eq!(world_name, "complex-processor");
        assert!(dts.contains("export interface Payload {"));
        assert!(dts.contains("id: number;"));
        assert!(dts.contains("title: string;"));
        assert!(dts.contains("tags: Array<string>;"));
        assert!(dts.contains("meta: string | null | undefined;"));
        assert!(dts.contains("export type Status = \"pending\" | \"active\" | \"failed\";"));
        assert!(dts.contains("export type OutputResult ="));
        assert!(dts.contains("tag: \"success\"; val: string"));
        assert!(dts.contains("tag: \"failure\"; val: number"));
        assert!(dts.contains(
            "export declare function process(data: Payload, stat: Status): OutputResult;"
        ));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
