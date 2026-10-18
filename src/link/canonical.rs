//! Allocator roots required by canonical lowering, independent of runtime implementation.
use crate::abi::world_functions;
use std::collections::BTreeSet;
use wit_parser::{
    Function, FutureIntrinsic, LiftLowerAbi, ManglingAndAbi, Resolve, StreamIntrinsic, Type,
    TypeDefKind, WasmImport, WorldId, WorldKey,
};

#[derive(Default)]
pub(crate) struct Requirements {
    pub(crate) export_realloc: bool,
    allocating_imports: BTreeSet<(String, String)>,
}

impl Requirements {
    pub(crate) fn for_world(resolve: &Resolve, world: WorldId) -> Self {
        let mut requirements = Self::default();
        for (exported, items) in [
            (false, &resolve.worlds[world].imports),
            (true, &resolve.worlds[world].exports),
        ] {
            for entry in world_functions(resolve, items.iter()) {
                requirements.function(
                    resolve,
                    entry.interface.map(|_| entry.key),
                    entry.function,
                    exported,
                );
            }
        }
        requirements
    }

    pub(crate) fn import(&self, module: &str, name: &str) -> bool {
        self.allocating_imports
            .iter()
            .any(|(m, n)| m == module && n == name)
    }

    fn function(
        &mut self,
        resolve: &Resolve,
        interface: Option<&WorldKey>,
        func: &Function,
        exported: bool,
    ) {
        let mangling = ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback).for_func(func);
        if exported {
            self.export_realloc |= resolve
                .wasm_signature(mangling.export_variant(), func)
                .indirect_params
                || func
                    .params
                    .iter()
                    .any(|param| lowers_allocation(resolve, param.ty));
        } else if func.result.is_some_and(|ty| lowers_allocation(resolve, ty)) {
            self.allocating_imports
                .insert(resolve.wasm_import_name(mangling, WasmImport::Func { interface, func }));
        }
        // A read lowers its payload into guest memory; writes lift existing storage.
        for ty in func.find_futures_and_streams(resolve) {
            let (payload, is_future) = match &resolve.types[ty].kind {
                TypeDefKind::Future(payload) => (*payload, true),
                TypeDefKind::Stream(payload) => (*payload, false),
                _ => continue,
            };
            if !payload.is_some_and(|ty| lowers_allocation(resolve, ty)) {
                continue;
            }
            for async_ in [false, true] {
                let import = if is_future {
                    WasmImport::FutureIntrinsic {
                        interface,
                        func,
                        ty: Some(ty),
                        intrinsic: FutureIntrinsic::Read,
                        exported,
                        async_,
                    }
                } else {
                    WasmImport::StreamIntrinsic {
                        interface,
                        func,
                        ty: Some(ty),
                        intrinsic: StreamIntrinsic::Read,
                        exported,
                        async_,
                    }
                };
                self.allocating_imports
                    .insert(resolve.wasm_import_name(mangling, import));
            }
        }
    }
}

/// Only strings and variable-length collections allocate when lowered. Handles do not
/// traverse their referents; future/stream payloads are handled at their read imports.
fn lowers_allocation(resolve: &Resolve, ty: Type) -> bool {
    let Type::Id(id) = ty else {
        return ty == Type::String;
    };
    match &resolve.types[id].kind {
        TypeDefKind::List(_) | TypeDefKind::Map(..) => true,
        TypeDefKind::Type(ty) | TypeDefKind::Option(ty) | TypeDefKind::FixedLengthList(ty, _) => {
            lowers_allocation(resolve, *ty)
        }
        TypeDefKind::Record(record) => record
            .fields
            .iter()
            .any(|field| lowers_allocation(resolve, field.ty)),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().any(|ty| lowers_allocation(resolve, *ty)),
        TypeDefKind::Result(result) => result
            .ok
            .iter()
            .chain(&result.err)
            .any(|ty| lowers_allocation(resolve, *ty)),
        TypeDefKind::Variant(variant) => variant
            .cases
            .iter()
            .filter_map(|case| case.ty)
            .any(|ty| lowers_allocation(resolve, ty)),
        TypeDefKind::Resource
        | TypeDefKind::Handle(_)
        | TypeDefKind::Flags(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_)
        | TypeDefKind::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_requirements_follow_direction_and_payload_types() -> anyhow::Result<()> {
        let mut resolve = Resolve::default();
        let package = resolve.push_str("allocation.wit", r#"
package test:allocation;
interface host {
  labels: func() -> list<string>;
  send: func(value: string);
  later: async func() -> future<string>;
  chunks: async func() -> stream<u8>;
}
world output { import host; export run: func() -> string; }
world input { record data { label: option<string> } export run: func(value: data); }
world indirect { export run: func(a:u32,b:u32,c:u32,d:u32,e:u32,f:u32,g:u32,h:u32,i:u32,j:u32,k:u32,l:u32,m:u32,n:u32,o:u32,p:u32,q:u32); }
"#)?;
        let world = resolve.select_world(&[package], Some("output"))?;
        let requirements = Requirements::for_world(&resolve, world);
        assert!(
            !requirements.export_realloc,
            "lifting output strings borrows guest memory"
        );
        assert!(requirements.import("test:allocation/host", "labels"));
        assert!(!requirements.import("test:allocation/host", "send"));
        assert!(!requirements.import("test:allocation/host", "[async-lower]later"));
        assert!(requirements.import("test:allocation/host", "[async-lower][future-read-0]later"));
        assert!(!requirements.import("test:allocation/host", "[async-lower][stream-read-0]chunks"));
        for name in ["input", "indirect"] {
            let world = resolve.select_world(&[package], Some(name))?;
            assert!(
                Requirements::for_world(&resolve, world).export_realloc,
                "{name}"
            );
        }
        Ok(())
    }
}
