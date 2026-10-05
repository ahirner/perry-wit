//! Finite filesystem option records share typed validation and preserve source effects.
use crate::waffle_backend::{
    runtime::builder::{self, Builder},
    strings::StringPool,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
};

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    compare: Func,
    keys: &StringPool,
    functions: &mut BTreeMap<String, Func>,
) -> Result<()> {
    let label = builder::declare(module, "fs.option-label", &[I32; 2], &[I32]);
    let mut b = Builder::new(module, label, memory);
    let tag = b.load(b.param(0), 8, I32);
    let one = b.integer(1);
    let omitted = b.op(O::I32LeU, &[tag, one], I32);
    let default = b.op(O::I32And, &[omitted, b.param(1)], I32);
    let empty = b.body.add_block();
    let value = b.body.add_block();
    b.branch(default, empty, value);
    b.block = empty;
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.block = value;
    let string = b.integer(4);
    let string = b.op(O::I32Eq, &[tag, string], I32);
    let present = b.body.add_block();
    let invalid = b.body.add_block();
    b.branch(string, present, invalid);
    b.block = invalid;
    let invalid = b.integer(u32::MAX);
    b.ret(&[invalid]);
    b.block = present;
    let payload = b.load(b.param(0), 16, F64);
    let pointer = b.op(O::I32TruncF64U, &[payload], I32);
    b.ret(&[pointer]);
    b.finish(module, label)?;

    for write in [false, true] {
        let name = if write {
            "fs.write-object-options"
        } else {
            "fs.read-object-options"
        };
        let function =
            builder::declare(module, name, &vec![I32; if write { 2 } else { 1 }], &[I32]);
        let mut b = Builder::new(module, function, memory);
        let zero = b.integer(0);
        let one = b.integer(1);
        let entry = b.load(b.param(0), 0, I32);
        let next = b.body.add_block();
        let current = b.body.add_blockparam(next, I32);
        let encoding = b.body.add_blockparam(next, I32);
        let flag = b.body.add_blockparam(next, I32);
        let valid = b.body.add_blockparam(next, I32);
        let field = b.body.add_block();
        let done = b.body.add_block();
        b.jump(next, &[entry, zero, zero, one]);
        b.block = next;
        b.branch(current, field, done);
        b.block = field;
        let key = b.load(current, 4, I32);
        let following = b.load(current, 0, I32);
        for name in ["encoding", "flag", "signal"] {
            let expected = b.integer(keys.get(name).unwrap());
            let mismatch = b.call(compare, &[key, expected], &[I32])[0];
            let matched = b.body.add_block();
            let other = b.body.add_block();
            b.branch(mismatch, other, matched);
            b.block = matched;
            let value = if name == "signal" {
                zero
            } else {
                b.call(
                    label,
                    &[current, if name == "encoding" { one } else { zero }],
                    &[I32],
                )[0]
            };
            b.jump(
                next,
                &[
                    following,
                    if name == "encoding" { value } else { encoding },
                    if name == "flag" { value } else { flag },
                    valid,
                ],
            );
            b.block = other;
        }
        b.jump(next, &[following, encoding, flag, zero]);
        b.block = done;
        let result = if write {
            let result = b.call(
                functions["fs.write-options"],
                &[encoding, flag, b.param(1)],
                &[I32],
            )[0];
            b.op(O::I32And, &[result, valid], I32)
        } else {
            b.call(
                functions["fs.read-options"],
                &[encoding, flag, valid],
                &[I32],
            )[0]
        };
        b.ret(&[result]);
        b.finish(module, function)?;
        functions.insert(name.into(), function);
    }

    let function = builder::declare(module, "fs.metadata-object-options", &[I32; 2], &[I32]);
    let mut b = Builder::new(module, function, memory);
    let operation = b.param(1);
    let one = b.integer(1);
    let supported = b.op(O::I32LeU, &[operation, one], I32);
    let invalid = b.body.add_block();
    let start = b.body.add_block();
    b.branch(supported, start, invalid);
    b.block = invalid;
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.block = start;
    let entry = b.load(b.param(0), 0, I32);
    let next = b.body.add_block();
    let current = b.body.add_blockparam(next, I32);
    let field = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[entry]);
    b.block = next;
    b.branch(current, field, done);
    b.block = done;
    b.ret(&[one]);
    b.block = field;
    let key = b.load(current, 4, I32);
    let following = b.load(current, 0, I32);
    for (name, directory, expected) in [
        ("encoding", true, 0),
        ("recursive", true, 0),
        ("withFileTypes", true, 0),
        ("bigint", false, 0),
        ("throwIfNoEntry", false, 1),
    ] {
        let candidate = b.integer(keys.get(name).unwrap());
        let comparison = b.call(compare, &[key, candidate], &[I32])[0];
        let equal = b.op(O::I32Eqz, &[comparison], I32);
        let mode = b.integer(u32::from(directory));
        let mode = b.op(O::I32Eq, &[operation, mode], I32);
        let matches = b.op(O::I32And, &[equal, mode], I32);
        let matched = b.body.add_block();
        let other = b.body.add_block();
        b.branch(matches, matched, other);
        b.block = matched;
        let valid = if name == "encoding" {
            let label = b.call(label, &[current, one], &[I32])[0];
            let encoding = b.call(functions["fs.encoding"], &[label], &[I32])[0];
            b.op(O::I32Eqz, &[encoding], I32)
        } else {
            let tag = b.load(current, 8, I32);
            let boolean = b.integer(2);
            let boolean = b.op(O::I32Eq, &[tag, boolean], I32);
            let payload = b.load(current, 16, F64);
            let expected = b.number(expected as f64);
            let value = b.op(O::F64Eq, &[payload, expected], I32);
            b.op(O::I32And, &[boolean, value], I32)
        };
        let advance = b.body.add_block();
        b.branch(valid, advance, invalid);
        b.block = advance;
        b.jump(next, &[following]);
        b.block = other;
    }
    b.jump(invalid, &[]);
    b.finish(module, function)?;
    functions.insert("fs.metadata-object-options".into(), function);
    Ok(())
}
