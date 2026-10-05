//! Callback-owned HTTP operations over the official canonical async intrinsics.
use crate::waffle_backend::runtime::{
    builder::{self, Builder},
    imports,
    operations::{Cancellation, Operations, Transfer},
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module, Operator as O, Type::I32};

const TRANSFERS: [(&str, u32, usize); 6] = [
    ("read", 2, 3),
    ("write", 3, 3),
    ("read-trailers", 4, 2),
    ("read-completion", 4, 2),
    ("write-trailers", 5, 2),
    ("write-completion", 5, 2),
];

pub(crate) fn declare(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    let mut functions = vec![imports::Function {
        name: "async-send".into(),
        params: vec!["i32"; 2],
        results: vec!["i32"],
    }];
    for (name, event, arity) in TRANSFERS {
        functions.push(imports::Function {
            name: format!("async-{name}"),
            params: vec!["i32"; arity],
            results: vec!["i32"],
        });
        if event != 5 {
            functions.push(imports::Function {
                name: format!("cancel-{name}"),
                params: vec!["i32"],
                results: vec!["i32"],
            });
        }
    }
    imports::declare_imports(module, "http", &functions)
}

pub(crate) fn controllers(native: &BTreeMap<String, Func>) -> Vec<Transfer> {
    TRANSFERS
        .iter()
        .map(|(name, event, _)| Transfer {
            event: *event,
            cancellation: if *event == 5 {
                Cancellation::Complete
            } else {
                Cancellation::Cancel(native[&format!("cancel-{name}")])
            },
        })
        .collect()
}

fn failed_result(b: &mut Builder, status: waffle::Value, out: waffle::Value) {
    let failed = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(status, failed, ready);
    b.block = failed;
    let zero = b.integer(0);
    let size = b.integer(64);
    b.effect(
        O::MemoryFill {
            mem: b.memory(0).memory,
        },
        &[out, zero, size],
    );
    let one = b.integer(1);
    let internal_error = b.integer(25);
    b.store(out, 0, one, I32);
    b.store(out, 8, internal_error, I32);
    b.jump(ready, &[]);
    b.block = ready;
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    native: &mut BTreeMap<String, Func>,
    operations: Operations,
) -> Result<()> {
    for (index, (name, event, arity)) in TRANSFERS.into_iter().enumerate() {
        let function = builder::declare(
            module,
            &format!("http.owned-{name}"),
            &vec![I32; arity],
            &[I32],
        );
        let mut b = Builder::new(module, function, memory);
        let args = (0..arity).map(|index| b.param(index)).collect::<Vec<_>>();
        let status = b.call(native[&format!("async-{name}")], &args, &[I32])[0];
        let controller = b.integer(index as u32 + 1);
        let owner = b.call(
            operations.register_transfer.unwrap(),
            &[args[0], status, controller],
            &[I32],
        )[0];
        let status = if event == 5 {
            owner
        } else {
            b.call(operations.wait, &[owner], &[I32])[0]
        };
        let status = if event == 4 {
            // A cancelled read has no initialized WIT result to lift.
            failed_result(&mut b, status, args[1]);
            b.integer(0)
        } else {
            status
        };
        b.ret(&[status]);
        b.finish(module, function)?;
        native.insert(name.into(), function);
    }
    let function = builder::declare(module, "http.owned-send", &[I32; 2], &[]);
    let mut b = Builder::new(module, function, memory);
    let task = b.call(native["async-send"], &[b.param(0), b.param(1)], &[I32])[0];
    let owner = b.call(operations.register, &[task], &[I32])[0];
    let status = b.call(operations.wait, &[owner], &[I32])[0];
    let returned = b.integer(2);
    let failed = b.op(O::I32Ne, &[status, returned], I32);
    let out = b.param(1);
    failed_result(&mut b, failed, out);
    b.ret(&[]);
    b.finish(module, function)?;
    native.insert("send".into(), function);

    let function = builder::declare(module, "http.owned-finish-write", &[I32; 3], &[]);
    let mut b = Builder::new(module, function, memory);
    let status = b.call(operations.wait, &[b.param(1)], &[I32])[0];
    let one = b.integer(1);
    let valid = b.op(O::I32LeU, &[status, one], I32);
    b.require(valid);
    b.ret(&[]);
    b.finish(module, function)?;
    native.insert("finish-write".into(), function);
    Ok(())
}
