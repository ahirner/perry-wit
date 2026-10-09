//! HTTP handlers: a program implementing `perry:http-handler` (`handle(request): response`
//! over plain WIT records, the SDK's generated types) compiles to a component exporting
//! `wasi:http/handler@0.3.0`.
//!
//! An adapter, compiled with the program, is the export: it reads the `wasi:http` request
//! (its parts at once, its body to the end), calls `handle` with the record, and builds the
//! `wasi:http` response from the returned one through the services of `perry:http-server`
//! (`perry-web`'s `serve` module). The response body is written after `task.return`, so the
//! export runs until the host has read it.
use super::{Compiled, bind, finish, lower, wit_types};
use crate::component;
use anyhow::{Context as _, Result};
use perry_codegen::{
    Exports,
    boundary::{AbiType, Export, Flat},
};
use pliron::context::Context;
use wit_parser::{
    Resolve, Type, TypeDefKind, WorldId, WorldItem,
    abi::{FlatTypes, WasmType},
};

/// The world programs implement.
pub(crate) fn is_handler_world(resolve: &Resolve, world: WorldId) -> bool {
    let world = &resolve.worlds[world];
    world.name == "handler"
        && world.package.is_some_and(|package| {
            let name = &resolve.packages[package].name;
            name.namespace == "perry" && name.name == "http-handler"
        })
}

/// The component's world.
const NATIVE: &str = "package perry:compiler-http;
world handler {
  include wasi:cli/imports@0.3.0;
  export wasi:http/handler@0.3.0;
}";

const ADAPTER: &str = r#"
import { request as __perryRequest, body as __perryBody, method as __perryMethod,
  methodOther as __perryMethodOther, hasScheme as __perryHasScheme, scheme as __perryScheme,
  schemeOther as __perrySchemeOther, authority as __perryAuthority, path as __perryPath,
  headerCount as __perryHeaderCount, headerName as __perryHeaderName,
  headerValue as __perryHeaderValue, addHeader as __perryAddHeader,
  respond as __perryRespond } from "perry:http-server";

export async function __perryHttpHandle(__perryHandle: number): Promise<number> {
  const incoming = __perryRequest(__perryHandle);
  const body = await __perryBody(incoming);
  const headers: [string, Uint8Array][] = [];
  const count = __perryHeaderCount(incoming);
  for (let i = 0; i < count; i++) {
    headers.push([__perryHeaderName(incoming, i), __perryHeaderValue(incoming, i)]);
  }
  const scheme: { tag: string; val: string | undefined } | undefined = __perryHasScheme(incoming)
    ? { tag: __perryScheme(incoming), val: __perrySchemeOther(incoming) }
    : undefined;
  const response = AWAIT handle({
    method: { tag: __perryMethod(incoming), val: __perryMethodOther(incoming) },
    scheme,
    authority: __perryAuthority(incoming),
    pathWithQuery: __perryPath(incoming),
    headers,
    body,
  });
  for (let i = 0; i < response.headers.length; i++) {
    const header = response.headers[i];
    __perryAddHeader(incoming, header[0], header[1]);
  }
  return __perryRespond(incoming, response.status, response.body);
}
"#;

/// Compile a `perry:http-handler` program (`types`: the resolve its SDK types come from)
/// to a `wasi:http/handler` component.
pub(crate) fn compile(
    source: &str,
    file_name: &str,
    types: &Resolve,
    componentize: bool,
) -> Result<Compiled> {
    let asynchronous = source.contains("async function handle");
    let adapter = ADAPTER.replace("AWAIT ", if asynchronous { "await " } else { "" });
    let program = format!("{source}\n{adapter}");
    let ctx = &mut Context::new();
    let mir = lower(ctx, &program, file_name, &wit_types::foreign(types))?;
    let (resolve, package) = component::wit::resolve_source("perry-http-handler.wit", NATIVE)?;
    let world = resolve.select_world(&[package], Some("handler"))?;
    let (key, interface) = resolve.worlds[world]
        .exports
        .iter()
        .find_map(|(key, item)| match item {
            WorldItem::Interface { id, .. } => Some((key, *id)),
            _ => None,
        })
        .context("the handler world exports wasi:http/handler")?;
    let name = resolve.name_world_key(key);
    let handle = &resolve.interfaces[interface].functions["handle"];
    let result = handle.result.context("handle returns a result")?;
    let exports = vec![Export {
        core_name: format!("{name}#handle"),
        function: "__perryHttpHandle".into(),
        params: vec![AbiType::U32],
        result: Some(AbiType::OkHandle {
            flat: flat(&resolve, &result),
            error_case: internal_error(&resolve, &result)?,
        }),
        task_return: Some((format!("[export]{name}"), "handle".into())),
        command: false,
    }];
    let imports = bind::imports(&resolve, world);
    let streams = bind::stream_anchor(&resolve, world);
    let core = perry_codegen::compile(ctx, &mir, &Exports::Canonical { exports, imports })?;
    let core = finish(core, streams.as_ref())?;
    let (component, stripped) = if componentize {
        let component = component::encode_resolved(&core, &resolve, world)?;
        let stripped = crate::strip::component(&component)?;
        (Some(component), Some(stripped))
    } else {
        (None, None)
    };
    Ok(Compiled {
        core,
        component,
        stripped,
    })
}

/// The flat core types of `ty` (at most the 16 that `task.return` takes directly).
fn flat(resolve: &Resolve, ty: &Type) -> Vec<Flat> {
    let mut storage = [WasmType::I32; 17];
    let mut flat = FlatTypes::new(&mut storage);
    resolve.push_flat(ty, &mut flat);
    flat.to_vec()
        .into_iter()
        .map(|ty| match ty {
            WasmType::I64 | WasmType::PointerOrI64 => Flat::I64,
            WasmType::F32 => Flat::F32,
            WasmType::F64 => Flat::F64,
            _ => Flat::I32,
        })
        .collect()
}

/// The case of `error-code` (the `err` of `result`) that reports a handler's exception:
/// `internal-error(option<string>)`.
fn internal_error(resolve: &Resolve, result: &Type) -> Result<u32> {
    let mut ty = *result;
    let mut in_error = false;
    while let Type::Id(id) = ty {
        match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => ty = *inner,
            TypeDefKind::Result(result) if !in_error => {
                ty = result.err.context("handle's result has an error type")?;
                in_error = true;
            }
            TypeDefKind::Variant(variant) => {
                return variant
                    .cases
                    .iter()
                    .position(|case| case.name == "internal-error")
                    .map(|index| index as u32)
                    .context("error-code has internal-error");
            }
            _ => break,
        }
    }
    anyhow::bail!("handle's error type is not error-code")
}
