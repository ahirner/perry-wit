//! Async descriptor calls publish valid errors only after subtask cancellation is acknowledged.
use crate::waffle_backend::runtime::{
    builder::{self, Builder},
    imports,
    operations::Operations,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module, Operator as O, Type::I32};

pub(super) const CALLS: &[(&str, usize)] = &[
    ("open", 7),
    ("stat", 5),
    ("mkdir", 4),
    ("unlink", 4),
    ("rmdir", 4),
];

pub(super) fn declare(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    let functions = CALLS
        .iter()
        .map(|(name, arity)| imports::Function {
            name: format!("async-{name}"),
            params: vec![
                "i32";
                if arity - 1 > wit_parser::Resolve::MAX_FLAT_ASYNC_PARAMS {
                    2
                } else {
                    *arity
                }
            ],
            results: vec!["i32"],
        })
        .collect::<Vec<_>>();
    imports::declare_imports(module, "filesystem", &functions)
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    native: &mut BTreeMap<String, Func>,
    operations: Operations,
    allocator: crate::waffle_backend::allocation::AllocationFuncs,
) -> Result<()> {
    for &(name, arity) in CALLS {
        let function = builder::declare(
            module,
            &format!("filesystem.owned-{name}"),
            &vec![I32; arity],
            &[],
        );
        let mut b = Builder::new(module, function, memory);
        let args = (0..arity).map(|index| b.param(index)).collect::<Vec<_>>();
        let failed = b.body.add_block();
        let start = b.body.add_block();
        let done = b.body.add_block();
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        b.branch(aborted, failed, start);
        b.block = start;
        let indirect = arity - 1 > wit_parser::Resolve::MAX_FLAT_ASYNC_PARAMS;
        let frame = if indirect {
            let one = b.integer(1);
            Some(b.call(allocator.frame_new, &[one], &[I32])[0])
        } else {
            None
        };
        let lowered = if let Some(frame) = frame {
            let pointer = b.allocate(allocator.realloc, 4 * (arity as u32 - 1), 4);
            b.store(frame, 12, pointer, I32);
            for (index, value) in args[..arity - 1].iter().enumerate() {
                b.store(pointer, index as u32 * 4, *value, I32);
            }
            vec![pointer, args[arity - 1]]
        } else {
            args.clone()
        };
        let task = b.call(native[&format!("async-{name}")], &lowered, &[I32])[0];
        let owner = b.call(operations.register, &[task], &[I32])[0];
        let status = b.call(operations.wait, &[owner], &[I32])[0];
        if let Some(frame) = frame {
            b.call(allocator.frame_drop, &[frame], &[]);
        }
        let returned = b.integer(2);
        let success = b.op(O::I32Eq, &[status, returned], I32);
        b.branch(success, done, failed);
        b.block = failed;
        let one = b.integer(1);
        let interrupted = b.integer(10);
        let out = args[arity - 1];
        b.store(out, 0, one, I32);
        b.store(out, if name == "stat" { 8 } else { 4 }, interrupted, I32);
        b.jump(done, &[]);
        b.block = done;
        b.ret(&[]);
        b.finish(module, function)?;
        native.insert(name.into(), function);
    }
    Ok(())
}
