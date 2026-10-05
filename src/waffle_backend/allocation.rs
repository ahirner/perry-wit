//! Managed invocation storage, canonical allocation, and typed graph tracing.

mod lookup;
mod roots;
pub(crate) mod scope;
pub(crate) use roots::{RetainedValues, track_roots};

use std::collections::BTreeMap;

use anyhow::Result;
use waffle::{
    Block, BlockTarget, Export, ExportKind, Func, FuncDecl, FunctionBody, Memory, MemoryArg,
    Module, Operator, SignatureData, Terminator, Type, Value,
};

pub(crate) const PAGE_BYTES: u32 = 65_536;

#[derive(Debug, Clone, Copy)]
pub(crate) struct AllocationFuncs {
    pub(crate) realloc: Func,
    pub(crate) frame_new: Func,
    pub(crate) frame_drop: Func,
    pub(crate) collect: Func,
    pub(crate) retained_frame_new: Func,
    pub(crate) post_return: Func,
}

/// Discriminants shared with allocation/runtime.wat's precise object scanner.
pub(crate) enum AllocationKind {
    String = 1,
    StringArray = 2,
    ScalarPromise = 3,
    ReferencePromise = 4,
    CanonicalValue = 19,
}

pub(crate) fn emit_allocator(
    module: &mut Module<'static>,
    memory: Memory,
    heap_base: u32,
) -> Result<AllocationFuncs> {
    let bump = emit_bump_allocator(module, memory, heap_base)?;
    let (find, index) = lookup::emit(module, memory)?;
    let functions = super::runtime::emit_functions(
        module,
        memory,
        include_str!("allocation/runtime.wat"),
        &BTreeMap::from([("bump", bump), ("find", find), ("index", index)]),
    )?;
    let realloc = functions["cabi_realloc"];
    module.exports.push(Export {
        name: "cabi_realloc".into(),
        kind: ExportKind::Func(realloc),
    });
    Ok(AllocationFuncs {
        realloc,
        frame_new: functions["frame-new"],
        frame_drop: functions["frame-drop"],
        collect: functions["collect"],
        retained_frame_new: functions["retained-frame-new"],
        post_return: functions["post-return"],
    })
}

pub(crate) fn tag_allocation(
    body: &mut FunctionBody,
    block: Block,
    memory: Memory,
    pointer: Value,
    kind: AllocationKind,
) {
    let four = body.add_op(block, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
    let backlink = body.add_op(block, Operator::I32Sub, &[pointer, four], &[Type::I32]);
    let header = body.add_op(
        block,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[backlink],
        &[Type::I32],
    );
    let kind = body.add_op(
        block,
        Operator::I32Const { value: kind as u32 },
        &[],
        &[Type::I32],
    );
    body.add_op(
        block,
        Operator::I32Store {
            memory: MemoryArg {
                align: 2,
                offset: 16,
                memory,
            },
        },
        &[header, kind],
        &[],
    );
}

/// Releases invocation storage after the canonical caller has copied the result.
/// All source frames have exited and native completion has drained pending tasks.
/// Instance-retained roots survive; otherwise the entire arena can be reset.
pub(crate) fn emit_post_return(
    module: &mut Module<'static>,
    allocator: AllocationFuncs,
    export: &crate::waffle_backend::registry::FunctionExport,
) -> Result<()> {
    let sig = module.signatures.push(SignatureData {
        params: module.signatures[export.sig].returns.clone(),
        returns: vec![],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    body.add_op(
        entry,
        Operator::Call {
            function_index: allocator.post_return,
        },
        &[],
        &[],
    );
    body.set_terminator(entry, Terminator::Return { values: vec![] });
    body.validate()?;
    let name = format!("cabi_post_{}", export.name);
    let func = module.funcs.push(FuncDecl::Body(sig, name.clone(), body));
    module.exports.push(Export {
        name,
        kind: ExportKind::Func(func),
    });
    Ok(())
}

/// Narrows only after checking sizes computed outside the allocator's i32 ABI.
pub(crate) fn checked_allocation_size(
    body: &mut FunctionBody,
    block: Block,
    bytes: Value,
) -> (Block, Value) {
    let limit = body.add_op(
        block,
        Operator::I64Const {
            value: u32::MAX as u64,
        },
        &[],
        &[Type::I64],
    );
    let overflow = body.add_op(block, Operator::I64GtU, &[bytes, limit], &[Type::I32]);
    let trap = body.add_block();
    let valid = body.add_block();
    body.set_terminator(trap, Terminator::Unreachable);
    body.set_terminator(
        block,
        Terminator::CondBr {
            cond: overflow,
            if_true: BlockTarget {
                block: trap,
                args: vec![],
            },
            if_false: BlockTarget {
                block: valid,
                args: vec![],
            },
        },
    );
    let size = body.add_op(valid, Operator::I32WrapI64, &[bytes], &[Type::I32]);
    (valid, size)
}

/// Allocates only after validating address arithmetic and growing memory successfully.
fn emit_bump_allocator(
    module: &mut Module<'static>,
    memory: Memory,
    heap_base: u32,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32; 4],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let params: Vec<_> = body.blocks[entry]
        .params
        .iter()
        .map(|&(_, value)| value)
        .collect();
    let [old_ptr, old_size, alignment, new_size] = params[..] else {
        unreachable!()
    };
    let trap = body.add_block();
    body.set_terminator(trap, Terminator::Unreachable);
    let capacity = body.add_block();
    let grow = body.add_block();
    let finish = body.add_block();

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let old_bump = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[zero],
        &[Type::I32],
    );
    let unused = body.add_op(entry, Operator::I32Eqz, &[old_bump], &[Type::I32]);
    let base = body.add_op(
        entry,
        Operator::I32Const { value: heap_base },
        &[],
        &[Type::I32],
    );
    let bump = body.add_op(
        entry,
        Operator::Select,
        &[base, old_bump, unused],
        &[Type::I32],
    );
    let bump = body.add_op(entry, Operator::I64ExtendI32U, &[bump], &[Type::I64]);
    let align = body.add_op(entry, Operator::I64ExtendI32U, &[alignment], &[Type::I64]);
    let size = body.add_op(entry, Operator::I64ExtendI32U, &[new_size], &[Type::I64]);
    let one = body.add_op(entry, Operator::I64Const { value: 1 }, &[], &[Type::I64]);
    let zero64 = body.add_op(entry, Operator::I64Const { value: 0 }, &[], &[Type::I64]);
    let mask = body.add_op(entry, Operator::I64Sub, &[align, one], &[Type::I64]);
    let padded = body.add_op(entry, Operator::I64Add, &[bump, mask], &[Type::I64]);
    let neg_align = body.add_op(entry, Operator::I64Sub, &[zero64, align], &[Type::I64]);
    let ptr64 = body.add_op(entry, Operator::I64And, &[padded, neg_align], &[Type::I64]);
    let end64 = body.add_op(entry, Operator::I64Add, &[ptr64, size], &[Type::I64]);
    let max_address = body.add_op(
        entry,
        Operator::I64Const {
            value: u32::MAX as u64,
        },
        &[],
        &[Type::I64],
    );
    let overflow = body.add_op(entry, Operator::I64GtU, &[end64, max_address], &[Type::I32]);
    let align_zero = body.add_op(entry, Operator::I64Eqz, &[align], &[Type::I32]);
    let align_bits = body.add_op(entry, Operator::I64And, &[align, mask], &[Type::I64]);
    let align_invalid = body.add_op(entry, Operator::I64Ne, &[align_bits, zero64], &[Type::I32]);
    let bad_alignment = body.add_op(
        entry,
        Operator::I32Or,
        &[align_zero, align_invalid],
        &[Type::I32],
    );
    let invalid = body.add_op(
        entry,
        Operator::I32Or,
        &[overflow, bad_alignment],
        &[Type::I32],
    );
    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: invalid,
            if_true: BlockTarget {
                block: trap,
                args: vec![],
            },
            if_false: BlockTarget {
                block: capacity,
                args: vec![],
            },
        },
    );

    let page_mask = body.add_op(
        capacity,
        Operator::I64Const {
            value: (PAGE_BYTES - 1) as u64,
        },
        &[],
        &[Type::I64],
    );
    let rounded_end = body.add_op(
        capacity,
        Operator::I64Add,
        &[end64, page_mask],
        &[Type::I64],
    );
    let shift = body.add_op(
        capacity,
        Operator::I64Const { value: 16 },
        &[],
        &[Type::I64],
    );
    let pages64 = body.add_op(
        capacity,
        Operator::I64ShrU,
        &[rounded_end, shift],
        &[Type::I64],
    );
    let pages = body.add_op(capacity, Operator::I32WrapI64, &[pages64], &[Type::I32]);
    let current = body.add_op(
        capacity,
        Operator::MemorySize { mem: memory },
        &[],
        &[Type::I32],
    );
    let needs_growth = body.add_op(capacity, Operator::I32GtU, &[pages, current], &[Type::I32]);
    body.set_terminator(
        capacity,
        Terminator::CondBr {
            cond: needs_growth,
            if_true: BlockTarget {
                block: grow,
                args: vec![],
            },
            if_false: BlockTarget {
                block: finish,
                args: vec![],
            },
        },
    );
    let delta = body.add_op(grow, Operator::I32Sub, &[pages, current], &[Type::I32]);
    let previous = body.add_op(
        grow,
        Operator::MemoryGrow { mem: memory },
        &[delta],
        &[Type::I32],
    );
    let failure = body.add_op(
        grow,
        Operator::I32Const { value: u32::MAX },
        &[],
        &[Type::I32],
    );
    let failed = body.add_op(grow, Operator::I32Eq, &[previous, failure], &[Type::I32]);
    body.set_terminator(
        grow,
        Terminator::CondBr {
            cond: failed,
            if_true: BlockTarget {
                block: trap,
                args: vec![],
            },
            if_false: BlockTarget {
                block: finish,
                args: vec![],
            },
        },
    );

    let ptr = body.add_op(finish, Operator::I32WrapI64, &[ptr64], &[Type::I32]);
    let end = body.add_op(finish, Operator::I32WrapI64, &[end64], &[Type::I32]);
    let shrinking = body.add_op(
        finish,
        Operator::I32LtU,
        &[new_size, old_size],
        &[Type::I32],
    );
    let copy_size = body.add_op(
        finish,
        Operator::Select,
        &[new_size, old_size, shrinking],
        &[Type::I32],
    );
    body.add_op(
        finish,
        Operator::MemoryCopy {
            dst_mem: memory,
            src_mem: memory,
        },
        &[ptr, old_ptr, copy_size],
        &[],
    );
    body.add_op(
        finish,
        Operator::I32Store {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[zero, end],
        &[],
    );
    body.set_terminator(finish, Terminator::Return { values: vec![ptr] });
    body.validate()?;
    body.verify_reducible()?;
    let func = module
        .funcs
        .push(FuncDecl::Body(sig, "heap.bump".into(), body));
    Ok(func)
}
