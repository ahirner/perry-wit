//! Canonical transfer adapters that retain buffers until the shared owner acknowledges completion.
use super::{
    builder::{self, Builder},
    imports,
    operations::{Cancellation, Operations, Transfer},
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module, Operator as O, Type::I32};

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Read,
    Write,
    Completion,
}
impl Kind {
    fn event(self) -> u32 {
        match self {
            Self::Read => 2,
            Self::Write => 3,
            Self::Completion => 4,
        }
    }
    fn arity(self) -> usize {
        if matches!(self, Self::Completion) {
            2
        } else {
            3
        }
    }
}
pub(crate) type Definition = (&'static str, Kind);

pub(crate) fn declare(
    module: &mut Module<'static>,
    namespace: &str,
    definitions: &[Definition],
) -> BTreeMap<String, Func> {
    let mut functions = Vec::new();
    for (name, kind) in definitions {
        functions.push(imports::Function {
            name: format!("async-{name}"),
            params: vec!["i32"; kind.arity()],
            results: vec!["i32"],
        });
        if !matches!(kind, Kind::Completion) {
            functions.push(imports::Function {
                name: format!("cancel-{name}"),
                params: vec!["i32"],
                results: vec!["i32"],
            });
        }
    }
    imports::declare_imports(module, namespace, &functions)
}
pub(crate) fn controllers(
    native: &BTreeMap<String, Func>,
    definitions: &[Definition],
) -> Vec<Transfer> {
    definitions
        .iter()
        .map(|(name, kind)| Transfer {
            event: kind.event(),
            cancellation: if matches!(kind, Kind::Completion) {
                Cancellation::Complete
            } else {
                Cancellation::Cancel(native[&format!("cancel-{name}")])
            },
        })
        .collect()
}
pub(crate) struct Adapters<'a> {
    pub(crate) namespace: &'a str,
    pub(crate) native: &'a mut BTreeMap<String, Func>,
    pub(crate) owners: Operations,
    pub(crate) first_index: u32,
}
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    adapters: Adapters<'_>,
    definitions: &[Definition],
) -> Result<()> {
    for (index, (name, kind)) in definitions.iter().enumerate() {
        let function = builder::declare(
            module,
            &format!("{}.owned-{name}", adapters.namespace),
            &vec![I32; kind.arity()],
            &[I32],
        );
        let mut b = Builder::new(module, function, memory);
        let args = (0..kind.arity())
            .map(|index| b.param(index))
            .collect::<Vec<_>>();
        if !matches!(kind, Kind::Completion) {
            let aborted = b.call(adapters.owners.aborted, &[], &[I32])[0];
            let cancelled = b.body.add_block();
            let transfer = b.body.add_block();
            b.branch(aborted, cancelled, transfer);
            b.block = cancelled;
            let status = b.integer(2);
            b.ret(&[status]);
            b.block = transfer;
        }
        let status = b.call(adapters.native[&format!("async-{name}")], &args, &[I32])[0];
        let controller = b.integer(adapters.first_index + index as u32 + 1);
        let owner = b.call(
            adapters.owners.register_transfer.unwrap(),
            &[args[0], status, controller],
            &[I32],
        )[0];
        let status = b.call(adapters.owners.wait, &[owner], &[I32])[0];
        if matches!(kind, Kind::Completion) {
            let complete = b.op(O::I32Eqz, &[status], I32);
            b.require(complete);
        }
        b.ret(&[status]);
        b.finish(module, function)?;
        adapters.native.insert((*name).into(), function);
    }
    Ok(())
}
