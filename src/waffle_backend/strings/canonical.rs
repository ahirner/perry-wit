//! Canonical UTF-8 input descriptors and scalar counts.

use super::descriptor::StringDescriptor;
use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type,
};

pub(super) fn emit_lift(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let ptr = body.blocks[entry].params[0].1;
    let byte_len = body.blocks[entry].params[1].1;

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);

    // Loop over byte_len to count Unicode scalars:
    // A byte is a scalar lead byte if (b & 0xC0) != 0x80.
    let loop_block = body.add_block();
    body.blocks[loop_block].desc = "lift_canonical count loop".into();
    let loop_idx = body.add_blockparam(loop_block, Type::I32);
    let loop_scalars = body.add_blockparam(loop_block, Type::I32);

    let done_block = body.add_block();
    body.blocks[done_block].desc = "lift_canonical done".into();
    let final_scalars = body.add_blockparam(done_block, Type::I32);

    body.set_terminator(
        entry,
        Terminator::Br {
            target: BlockTarget {
                block: loop_block,
                args: vec![zero, zero],
            },
        },
    );

    let is_done = body.add_op(
        loop_block,
        Operator::I32GeU,
        &[loop_idx, byte_len],
        &[Type::I32],
    );
    let step_block = body.add_block();
    body.blocks[step_block].desc = "lift_canonical step".into();

    body.set_terminator(
        loop_block,
        Terminator::CondBr {
            cond: is_done,
            if_true: BlockTarget {
                block: done_block,
                args: vec![loop_scalars],
            },
            if_false: BlockTarget {
                block: step_block,
                args: vec![],
            },
        },
    );

    let curr_addr = body.add_op(step_block, Operator::I32Add, &[ptr, loop_idx], &[Type::I32]);
    let b = body.add_op(
        step_block,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[curr_addr],
        &[Type::I32],
    );
    let c0 = body.add_op(
        step_block,
        Operator::I32Const { value: 0xC0 },
        &[],
        &[Type::I32],
    );
    let band = body.add_op(step_block, Operator::I32And, &[b, c0], &[Type::I32]);
    let c80 = body.add_op(
        step_block,
        Operator::I32Const { value: 0x80 },
        &[],
        &[Type::I32],
    );
    let is_lead = body.add_op(step_block, Operator::I32Ne, &[band, c80], &[Type::I32]);
    let next_scalars = body.add_op(
        step_block,
        Operator::I32Add,
        &[loop_scalars, is_lead],
        &[Type::I32],
    );

    let one = body.add_op(
        step_block,
        Operator::I32Const { value: 1 },
        &[],
        &[Type::I32],
    );
    let next_idx = body.add_op(step_block, Operator::I32Add, &[loop_idx, one], &[Type::I32]);

    body.set_terminator(
        step_block,
        Terminator::Br {
            target: BlockTarget {
                block: loop_block,
                args: vec![next_idx, next_scalars],
            },
        },
    );

    let desc_ptr = StringDescriptor {
        data_ptr: ptr,
        byte_len,
        scalar_len: final_scalars,
    }
    .allocate(&mut body, done_block, memory, cabi_realloc);

    body.set_terminator(
        done_block,
        Terminator::Return {
            values: vec![desc_ptr],
        },
    );
    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_lift_canonical".into(), body)))
}
