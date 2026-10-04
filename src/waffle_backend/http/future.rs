//! Shared trailers and completion transport for incoming and outgoing HTTP.

use crate::waffle_backend::runtime::{
    builder::{self, Builder},
    imports,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module, Operator as O, Type::I32};

pub(super) fn functions() -> Vec<imports::Function> {
    [
        ("new-trailers", vec![], vec!["i64"]),
        ("write-trailers", vec!["i32"; 2], vec!["i32"]),
        ("read-trailers", vec!["i32"; 2], vec!["i32"]),
        ("drop-trailers-reader", vec!["i32"], vec![]),
        ("drop-trailers-writer", vec!["i32"], vec![]),
        ("new-completion", vec![], vec!["i64"]),
        ("write-completion", vec!["i32"; 2], vec!["i32"]),
        ("read-completion", vec!["i32"; 2], vec!["i32"]),
        ("drop-completion-reader", vec!["i32"], vec![]),
        ("drop-completion-writer", vec!["i32"], vec![]),
        ("new-set", vec![], vec!["i32"]),
        ("join", vec!["i32"; 2], vec![]),
        ("wait", vec!["i32"; 2], vec!["i32"]),
        ("drop-set", vec!["i32"], vec![]),
    ]
    .into_iter()
    .map(|(name, params, results)| imports::Function {
        name: name.into(),
        params,
        results,
    })
    .collect()
}

pub(super) fn emit_finish_write(
    module: &mut Module<'static>,
    memory: Memory,
    native: &BTreeMap<String, Func>,
) -> Result<Func> {
    let function = builder::declare(module, "http.finish-write", &[I32, I32, I32], &[]);
    let mut b = Builder::new(module, function, memory);
    let writer = b.param(0);
    let status = b.param(1);
    let scratch = b.param(2);
    let blocked = b.integer(u32::MAX);
    let pending = b.op(O::I32Eq, &[status, blocked], I32);
    let wait = b.body.add_block();
    let complete = b.body.add_block();
    let terminal = b.body.add_blockparam(complete, I32);
    let immediate = b.body.add_block();
    b.branch(pending, wait, immediate);
    b.block = immediate;
    b.jump(complete, &[status]);
    b.block = wait;
    let set = b.call(native["new-set"], &[], &[I32])[0];
    b.call(native["join"], &[writer, set], &[]);
    let kind = b.call(native["wait"], &[set, scratch], &[I32])[0];
    let expected = b.integer(5);
    let valid = b.op(O::I32Eq, &[kind, expected], I32);
    b.require(valid);
    let handle = b.load(scratch, 0, I32);
    let valid = b.op(O::I32Eq, &[writer, handle], I32);
    b.require(valid);
    let status = b.load(scratch, 4, I32);
    let zero = b.integer(0);
    b.call(native["join"], &[writer, zero], &[]);
    b.call(native["drop-set"], &[set], &[]);
    b.jump(complete, &[status]);
    b.block = complete;
    let one = b.integer(1);
    let valid = b.op(O::I32LeU, &[terminal, one], I32);
    b.require(valid);
    b.ret(&[]);
    b.finish(module, function)?;
    Ok(function)
}
