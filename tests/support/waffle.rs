//! Application WIT for low-level compiler fixtures; production callers supply their own world.
#![allow(dead_code)]

use anyhow::{Result, bail};
use perry_hir::{ir::Module as HirModule, types::Type};
use perry_wit::waffle_backend::{self, WaffleCompileOptions, WaffleCompiled};

pub(crate) fn compile_typescript_waffle(
    source: &str,
    file: &str,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    let compiled = waffle_backend::compile_typescript(
        source,
        file,
        &WaffleCompileOptions {
            componentize: false,
            ..options.clone()
        },
    )?;
    component(compiled, options.componentize)
}

pub(crate) fn compile_hir(
    hir: &HirModule,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    let compiled = waffle_backend::compile_hir(
        hir,
        &WaffleCompileOptions {
            componentize: false,
            ..options.clone()
        },
    )?;
    component(compiled, options.componentize)
}

fn value_type(ty: &Type) -> Result<String> {
    Ok(match ty {
        Type::Number | Type::Any => "f64".into(),
        Type::Boolean => "bool".into(),
        Type::String => "string".into(),
        Type::Promise(inner) => value_type(inner)?,
        Type::Array(inner) if **inner == Type::String => "list<string>".into(),
        Type::Named(name) if name == "__perry_readable_bytes" => "stream<u8>".into(),
        Type::Named(name) if name == "Uint8Array" => "list<u8>".into(),
        Type::Named(name) if name.contains("Stats") => "stats".into(),
        Type::Named(name) if name.contains("value") => "f64".into(),
        Type::Union(types)
            if types.len() == 2
                && types.iter().all(
                    |ty| matches!(ty,Type::Object(shape) if shape.properties.contains_key("ok")),
                ) =>
        {
            let payload = |name| {
                types.iter().find_map(|ty| match ty {
                    Type::Object(shape) => shape.properties.get(name).map(|p| &p.ty),
                    _ => None,
                })
            };
            format!(
                "result<{},{}>",
                value_type(
                    payload("value")
                        .ok_or_else(|| anyhow::anyhow!("Fixture result requires value"))?
                )?,
                value_type(
                    payload("error")
                        .ok_or_else(|| anyhow::anyhow!("Fixture result requires error"))?
                )?
            )
        }
        Type::Union(types) if types.len() == 2 && types.contains(&Type::String) => {
            "text-or-bytes".into()
        }
        Type::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => format!(
            "result<{},{}>",
            value_type(&type_args[0])?,
            value_type(&type_args[1])?
        ),
        ty => bail!("Fixture needs an explicit WIT type for {ty:?}"),
    })
}

pub(crate) fn compile_typescript_for_fixture_world(
    source: &str,
    file: &str,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    let compiled = compile_typescript_waffle(
        source,
        file,
        &WaffleCompileOptions {
            componentize: false,
            ..options.clone()
        },
    )?;
    let (resolve, world) = fixture_world(&compiled)?;
    waffle_backend::compile_typescript_for_world(source, file, options, resolve, world)
}

fn fixture_world(compiled: &WaffleCompiled) -> Result<(wit_parser::Resolve, wit_parser::WorldId)> {
    let module = waffle::Module::from_wasm_bytes(&compiled.core, &Default::default())?;
    let mut wit = String::from("package test:compiler-fixture; world fixture {");
    if module.imports.iter().any(|import| {
        matches!(
            import.module.as_str(),
            "context" | "output" | "filesystem" | "random"
        ) || import.module.starts_with("wasi:")
    }) {
        wit.push_str("include wasi:cli/imports@0.3.0;");
    }
    if module.imports.iter().any(|import| import.module == "http") {
        wit.push_str("import wasi:http/client@0.3.0;");
    }
    if module
        .imports
        .iter()
        .any(|import| import.module == "host" && import.name == "hostDouble")
    {
        wit.push_str("import host-double: async func(value:f64)->f64;");
    }
    let function = compiled
        .hir
        .functions
        .iter()
        .find(|function| function.is_exported)
        .unwrap_or(&compiled.hir.functions[0]);
    let params = function
        .params
        .iter()
        .enumerate()
        .map(|(index, param)| Ok(format!("arg-{index}:{}", value_type(&param.ty)?)))
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let result = match &function.return_type {
        Type::Void => None,
        Type::Promise(inner) if **inner == Type::Void => None,
        ty => Some(value_type(ty)?),
    };
    let result = result.map(|ty| format!("->{ty}")).unwrap_or_default();
    if params.contains("text-or-bytes") || result.contains("text-or-bytes") {
        wit.push_str("variant text-or-bytes {text(string),bytes(list<u8>)}");
    }
    if params.contains("stats") || result.contains("stats") {
        wit.push_str("enum stats-kind {block-device,character-device,directory,fifo,symbolic-link,regular-file,socket,other} record stats {size:f64,mtime-ms:f64,kind:stats-kind}");
    }
    wit.push_str(&format!("export run:async func({params}){result};}}"));
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), wit)?;
    let (resolve, package) = perry_wit::component::wit::resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("fixture"))?;
    Ok((resolve, world))
}

fn component(mut compiled: WaffleCompiled, enabled: bool) -> Result<WaffleCompiled> {
    if !enabled {
        return Ok(compiled);
    }
    let (resolve, world) = fixture_world(&compiled)?;
    let component = waffle_backend::encode_component(&compiled.core, resolve, world)?;
    compiled.component_wat = Some(wasmprinter::print_bytes(&component)?);
    compiled.component = Some(component);
    Ok(compiled)
}
