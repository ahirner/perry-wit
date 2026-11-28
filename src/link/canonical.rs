//! Allocator roots required by canonical lowering, independent of runtime implementation.
use perry_contract::world_functions;
use std::collections::BTreeSet;
use wit_parser::{
    Function, FutureIntrinsic, LiftLowerAbi, ManglingAndAbi, Resolve, StreamIntrinsic, Type,
    TypeDefKind, TypeIdVisitor, WasmImport, WorldId, WorldKey,
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

/// Check whether lowering `ty` allocates in the guest. Strings and variable-length
/// collections allocate on lowering, whereas future and stream payloads allocate at their
/// read imports.
fn lowers_allocation(resolve: &Resolve, ty: Type) -> bool {
    struct Allocation(bool);
    impl TypeIdVisitor for Allocation {
        fn visit_type(&mut self, resolve: &Resolve, ty: &Type) {
            if self.0 {
                return;
            }
            match ty {
                Type::String => self.0 = true,
                Type::Id(id) => match resolve.types[*id].kind {
                    TypeDefKind::List(_) | TypeDefKind::Map(..) => self.0 = true,
                    TypeDefKind::Handle(_)
                    | TypeDefKind::Stream(_)
                    | TypeDefKind::Future(_)
                    | TypeDefKind::Unknown => {}
                    _ => self.visit_type_id(resolve, *id),
                },
                _ => {}
            }
        }
    }
    let mut allocation = Allocation(false);
    allocation.visit_type(resolve, &ty);
    allocation.0
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
