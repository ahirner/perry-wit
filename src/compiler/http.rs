//! HTTP handlers: a program implementing `perry:http-handler` (`handle(request): response`
//! over plain WIT records, the SDK's generated types) compiles to a component exporting
//! `wasi:http/handler@0.3.0`.
//!
//! An adapter, compiled with the program, is the export: it reads the `wasi:http` request
//! (its parts at once, its body to the end), calls `handle` with the record, and builds the
//! `wasi:http` response from the returned one through the services of `perry:http-server`
//! (`perry-web`'s `serve` module). The response body is written after `task.return`, so the
//! export runs until the host has read it.
use super::{Compiled, emit_component, lower};
use crate::component;
use perry_codegen::boundary::{AbiKind, Export, ExportKind};
use perry_wit_diagnostics::{Diagnostic, Result, bail_internal};
use pliron::context::Context;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

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
import type { Method as __PerryMethod,
  Scheme as __PerryScheme } from './handler-types/world';
import { request as __perryRequest, body as __perryBody, method as __perryMethod,
  methodOther as __perryMethodOther, hasScheme as __perryHasScheme, scheme as __perryScheme,
  schemeOther as __perrySchemeOther, authority as __perryAuthority, path as __perryPath,
  headerCount as __perryHeaderCount, headerName as __perryHeaderName,
  headerValue as __perryHeaderValue, addHeader as __perryAddHeader,
  respond as __perryRespond } from "perry:http-server";

function __perryRequestMethod(tag: string, other: string | undefined): __PerryMethod {
  switch (tag) {
    case 'get': return { tag: 'get' };
    case 'head': return { tag: 'head' };
    case 'post': return { tag: 'post' };
    case 'put': return { tag: 'put' };
    case 'delete': return { tag: 'delete' };
    case 'connect': return { tag: 'connect' };
    case 'options': return { tag: 'options' };
    case 'trace': return { tag: 'trace' };
    case 'patch': return { tag: 'patch' };
    default: return { tag: 'other', val: other! };
  }
}

function __perryRequestScheme(present: boolean, tag: string, other: string | undefined): __PerryScheme | null {
  if (!present) return null;
  switch (tag) {
    case 'http': return { tag: 'http' };
    case 'https': return { tag: 'https' };
    default: return { tag: 'other', val: other! };
  }
}

export async function __perryHttpHandle(__perryHandle: number): Promise<number> {
  const incoming = __perryRequest(__perryHandle);
  const body = await __perryBody(incoming);
  const headers: [string, Uint8Array][] = [];
  const count = __perryHeaderCount(incoming);
  for (let i = 0; i < count; i++) {
    headers.push([__perryHeaderName(incoming, i), __perryHeaderValue(incoming, i)]);
  }
  const response = AWAIT handle({
    method: __perryRequestMethod(__perryMethod(incoming), __perryMethodOther(incoming)),
    scheme: __perryRequestScheme(__perryHasScheme(incoming), __perryScheme(incoming), __perrySchemeOther(incoming)),
    authority: __perryAuthority(incoming) ?? null,
    pathWithQuery: __perryPath(incoming) ?? null,
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
    modules: &std::collections::BTreeMap<String, String>,
    types: &Resolve,
    types_world: WorldId,
    options: super::WorldOptions,
) -> Result<Compiled> {
    let asynchronous = source.contains("async function handle");
    let adapter = ADAPTER.replace("AWAIT ", if asynchronous { "await " } else { "" });
    let program = format!("{source}\n{adapter}");
    let ctx = &mut Context::new();
    let (mir, mut analyses) = lower(
        ctx,
        &program,
        file_name,
        modules,
        &perry_frontend::wit::WorldTypes::new(types, Some(types_world)),
        options.limits,
    )?;
    let (resolve, package) =
        component::wit::resolve_embedded_source("perry-http-handler.wit", NATIVE)?;
    let world = resolve
        .select_world(&[package], Some("handler"))
        .map_err(|err| {
            Diagnostic::internal_from("selecting embedded handler world", err.as_ref())
        })?;
    let (key, interface) = resolve.worlds[world]
        .exports
        .iter()
        .find_map(|(key, item)| match item {
            WorldItem::Interface { id, .. } => Some((key, *id)),
            _ => None,
        })
        .ok_or_else(|| Diagnostic::internal("the handler world exports wasi:http/handler"))?;
    let name = resolve.name_world_key(key);
    let handle = &resolve.interfaces[interface].functions["handle"];
    let result = handle
        .result
        .ok_or_else(|| Diagnostic::internal("handle returns a result"))?;
    let layouts = super::abi::Layouts::new(&resolve)?;
    let exports = vec![Export {
        source_name: None,
        annotations: Vec::new(),
        core_name: format!("{name}#handle"),
        function: "__perryHttpHandle".into(),
        signature: layouts.signature(handle, false),
        params: vec![layouts.value(Type::U32)?],
        indirect_size: None,
        result: Some(layouts.layout(
            result,
            AbiKind::OkHandle {
                error_case: internal_error(&resolve, &result)?,
            },
        )),
        kind: ExportKind::Async {
            task_return: (format!("[export]{name}"), "handle".into()),
            command: false,
        },
    }];
    emit_component(
        ctx,
        &mir,
        &mut analyses,
        &resolve,
        world,
        options.memory_checks,
        options.componentize,
        exports,
        vec![],
        vec![],
        false,
    )
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
                ty = result
                    .err
                    .ok_or_else(|| Diagnostic::internal("handle's result has an error type"))?;
                in_error = true;
            }
            TypeDefKind::Variant(variant) => {
                return variant
                    .cases
                    .iter()
                    .position(|case| case.name == "internal-error")
                    .map(|index| index as u32)
                    .ok_or_else(|| Diagnostic::internal("error-code has internal-error"));
            }
            _ => break,
        }
    }
    bail_internal!("handle's error type is not error-code")
}
