//! Selective P3 process context imports with instance-retained guest snapshots.

use super::{
    allocation::AllocationFuncs,
    capabilities::ContextOperation,
    objects::ObjectHelpers,
    runtime::{
        builder::{self, Builder},
        imports,
    },
    structured::StructuredHelpers,
};
use anyhow::Result;
use perry_hir::types::Type as HirType;
use std::collections::{BTreeMap, BTreeSet};
use waffle::{
    Func, Memory, Module, Operator as Op,
    Type::{F64, I32},
};

pub(crate) const ENVIRONMENT_TYPE: &str = "__perry_internal_environment";

pub(crate) fn is_environment(ty: &HirType) -> bool {
    matches!(ty,HirType::Named(name) if name == ENVIRONMENT_TYPE)
}

pub(crate) struct ContextHelpers {
    pub(crate) string_lift: Func,
    pub(crate) structured: Option<StructuredHelpers>,
    pub(crate) objects: Option<ObjectHelpers>,
}

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    operations: &BTreeSet<ContextOperation>,
) -> BTreeMap<String, Func> {
    imports::declare_imports(
        module,
        "context",
        &operations
            .iter()
            .map(|operation| imports::Function {
                name: operation.import().into(),
                params: vec!["i32"],
                results: vec![],
            })
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    operations: &BTreeSet<ContextOperation>,
    helpers: ContextHelpers,
) -> Result<BTreeMap<String, Func>> {
    let cache_function = builder::declare(module, "context.cache", &[], &[I32]);
    let mut b = Builder::new(module, cache_function, memory);
    let address = b.integer(76);
    let cache = b.load(address, 0, I32);
    let cached = b.body.add_block();
    let initialize = b.body.add_block();
    b.branch(cache, cached, initialize);
    b.block = cached;
    b.ret(&[cache]);
    b.block = initialize;
    let slots = b.integer(3);
    let cache = b.call(allocator.retained_frame_new, &[slots], &[I32])[0];
    b.store(address, 0, cache, I32);
    b.ret(&[cache]);
    b.finish(module, cache_function)?;

    let mut functions = BTreeMap::new();
    for operation in operations {
        let (name, slot, size) = match operation {
            ContextOperation::Arguments => ("process.argv", 12, 8),
            ContextOperation::InitialCwd => ("process.cwd", 16, 12),
            ContextOperation::Environment => ("process.env", 20, 8),
        };
        let function = builder::declare(module, name, &[], &[I32]);
        let mut b = Builder::new(module, function, memory);
        let cache = b.call(cache_function, &[], &[I32])[0];
        let value = b.load(cache, slot, I32);
        let cached = b.body.add_block();
        let initialize = b.body.add_block();
        b.branch(value, cached, initialize);
        b.block = cached;
        b.ret(&[value]);
        b.block = initialize;
        let result = b.allocate(allocator.realloc, size, 4);
        b.call(imports[operation.import()], &[result], &[]);
        let value = match operation {
            ContextOperation::Arguments => {
                let data = b.load(result, 0, I32);
                let count = b.load(result, 4, I32);
                b.call(
                    helpers
                        .structured
                        .expect("arguments require string array helpers")
                        .take_strings,
                    &[data, count],
                    &[I32],
                )[0]
            }
            ContextOperation::InitialCwd => {
                let present = b.op(
                    Op::I32Load8U {
                        memory: b.memory(0),
                    },
                    &[result],
                    I32,
                );
                let supplied = b.body.add_block();
                let absent = b.body.add_block();
                let lift = b.body.add_block();
                let data = b.body.add_blockparam(lift, I32);
                let length = b.body.add_blockparam(lift, I32);
                b.branch(present, supplied, absent);
                b.block = supplied;
                let pointer = b.load(result, 4, I32);
                let count = b.load(result, 8, I32);
                b.jump(lift, &[pointer, count]);
                b.block = absent;
                let pointer = b.allocate(allocator.realloc, 1, 1);
                let slash = b.integer(b'/' as u32);
                b.effect(
                    Op::I32Store8 {
                        memory: b.memory(0),
                    },
                    &[pointer, slash],
                );
                let one = b.integer(1);
                b.jump(lift, &[pointer, one]);
                b.block = lift;
                b.call(helpers.string_lift, &[data, length], &[I32])[0]
            }
            ContextOperation::Environment => {
                let data = b.load(result, 0, I32);
                let count = b.load(result, 4, I32);
                let maximum = b.integer(u32::MAX / 16);
                let valid = b.op(Op::I32LeU, &[count, maximum], I32);
                b.require(valid);
                let objects = helpers.objects.expect("environment requires objects");
                let value = b.call(objects.new, &[], &[I32])[0];
                let zero = b.integer(0);
                let one = b.integer(1);
                let string_tag = b.integer(super::values::ValueTag::String as u32);
                let width = b.integer(16);
                let next = b.body.add_block();
                let index = b.body.add_blockparam(next, I32);
                let entry = b.body.add_block();
                let done = b.body.add_block();
                b.jump(next, &[zero]);
                b.block = next;
                let end = b.op(Op::I32Eq, &[index, count], I32);
                b.branch(end, done, entry);
                b.block = entry;
                let offset = b.op(Op::I32Mul, &[index, width], I32);
                let pointer = b.op(Op::I32Add, &[data, offset], I32);
                let key_data = b.load(pointer, 0, I32);
                let key_len = b.load(pointer, 4, I32);
                let key = b.call(helpers.string_lift, &[key_data, key_len], &[I32])[0];
                let text_data = b.load(pointer, 8, I32);
                let text_len = b.load(pointer, 12, I32);
                let text = b.call(helpers.string_lift, &[text_data, text_len], &[I32])[0];
                let payload = b.op(Op::F64ConvertI32U, &[text], F64);
                let outcome = b.call(objects.set, &[value, key, string_tag, payload], &[I32, F64]);
                let valid = b.op(Op::I32Eqz, &[outcome[0]], I32);
                b.require(valid);
                let incremented = b.op(Op::I32Add, &[index, one], I32);
                b.jump(next, &[incremented]);
                b.block = done;
                let bytes = b.op(Op::I32Mul, &[count, width], I32);
                let four = b.integer(4);
                b.call(allocator.realloc, &[data, bytes, four, zero], &[I32]);
                b.store(value, 8, one, I32);
                value
            }
        };
        let size = b.integer(size);
        let four = b.integer(4);
        let zero = b.integer(0);
        b.call(allocator.realloc, &[result, size, four, zero], &[I32]);
        b.store(cache, slot, value, I32);
        b.ret(&[value]);
        b.finish(module, function)?;
        functions.insert(name.into(), function);
    }
    Ok(functions)
}
