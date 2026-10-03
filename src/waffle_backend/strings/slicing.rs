//! Slices translate scalar bounds to byte offsets without splitting UTF-8 sequences.

use super::descriptor::StringDescriptor;
use super::positions::{PositionMode, bounded_position};
use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type,
};

pub(super) fn emit_slice(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::F64, Type::F64],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let start_f64 = body.blocks[entry].params[1].1;
    let end_f64 = body.blocks[entry].params[2].1;

    let ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[desc],
        &[Type::I32],
    );
    let byte_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[desc],
        &[Type::I32],
    );
    let scalar_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 8,
                memory,
            },
        },
        &[desc],
        &[Type::I32],
    );

    let norm_start = bounded_position(
        &mut body,
        entry,
        start_f64,
        scalar_len,
        PositionMode::Relative,
    );
    let norm_end = bounded_position(
        &mut body,
        entry,
        end_f64,
        scalar_len,
        PositionMode::Relative,
    );
    let zero_i32 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);

    // If norm_start >= norm_end: return empty string descriptor
    let is_empty = body.add_op(
        entry,
        Operator::I32GeS,
        &[norm_start, norm_end],
        &[Type::I32],
    );
    let empty_block = body.add_block();
    body.blocks[empty_block].desc = "str_slice empty".into();
    let scan_block = body.add_block();
    body.blocks[scan_block].desc = "str_slice scan".into();

    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: is_empty,
            if_true: BlockTarget {
                block: empty_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: scan_block,
                args: vec![],
            },
        },
    );

    let empty_desc = StringDescriptor {
        data_ptr: ptr,
        byte_len: zero_i32,
        scalar_len: zero_i32,
    }
    .allocate(&mut body, empty_block, memory, cabi_realloc);
    body.set_terminator(
        empty_block,
        Terminator::Return {
            values: vec![empty_desc],
        },
    );

    // Scan block: iterate over byte_len to find byte offsets for norm_start and norm_end
    let loop_block = body.add_block();
    body.blocks[loop_block].desc = "str_slice loop".into();
    let loop_byte_idx = body.add_blockparam(loop_block, Type::I32);
    let loop_scalar_idx = body.add_blockparam(loop_block, Type::I32);
    let loop_start_byte = body.add_blockparam(loop_block, Type::I32);
    let loop_end_byte = body.add_blockparam(loop_block, Type::I32);

    body.set_terminator(
        scan_block,
        Terminator::Br {
            target: BlockTarget {
                block: loop_block,
                args: vec![zero_i32, zero_i32, zero_i32, byte_len],
            },
        },
    );

    let at_start = body.add_op(
        loop_block,
        Operator::I32Eq,
        &[loop_scalar_idx, norm_start],
        &[Type::I32],
    );
    let new_start_byte = body.add_op(
        loop_block,
        Operator::Select,
        &[loop_byte_idx, loop_start_byte, at_start],
        &[Type::I32],
    );

    let at_end = body.add_op(
        loop_block,
        Operator::I32Eq,
        &[loop_scalar_idx, norm_end],
        &[Type::I32],
    );
    let new_end_byte = body.add_op(
        loop_block,
        Operator::Select,
        &[loop_byte_idx, loop_end_byte, at_end],
        &[Type::I32],
    );

    let is_scan_done = body.add_op(
        loop_block,
        Operator::I32GeU,
        &[loop_byte_idx, byte_len],
        &[Type::I32],
    );
    let scan_done_block = body.add_block();
    body.blocks[scan_done_block].desc = "str_slice scan_done".into();
    let scan_step_block = body.add_block();
    body.blocks[scan_step_block].desc = "str_slice scan_step".into();

    body.set_terminator(
        loop_block,
        Terminator::CondBr {
            cond: is_scan_done,
            if_true: BlockTarget {
                block: scan_done_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: scan_step_block,
                args: vec![],
            },
        },
    );

    let curr_b_addr = body.add_op(
        scan_step_block,
        Operator::I32Add,
        &[ptr, loop_byte_idx],
        &[Type::I32],
    );
    let b_val = body.add_op(
        scan_step_block,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[curr_b_addr],
        &[Type::I32],
    );
    let c0 = body.add_op(
        scan_step_block,
        Operator::I32Const { value: 0xC0 },
        &[],
        &[Type::I32],
    );
    let band = body.add_op(
        scan_step_block,
        Operator::I32And,
        &[b_val, c0],
        &[Type::I32],
    );
    let c80 = body.add_op(
        scan_step_block,
        Operator::I32Const { value: 0x80 },
        &[],
        &[Type::I32],
    );
    let is_lead_b = body.add_op(scan_step_block, Operator::I32Ne, &[band, c80], &[Type::I32]);
    let next_sc = body.add_op(
        scan_step_block,
        Operator::I32Add,
        &[loop_scalar_idx, is_lead_b],
        &[Type::I32],
    );
    let one = body.add_op(
        scan_step_block,
        Operator::I32Const { value: 1 },
        &[],
        &[Type::I32],
    );
    let next_bi = body.add_op(
        scan_step_block,
        Operator::I32Add,
        &[loop_byte_idx, one],
        &[Type::I32],
    );

    body.set_terminator(
        scan_step_block,
        Terminator::Br {
            target: BlockTarget {
                block: loop_block,
                args: vec![next_bi, next_sc, new_start_byte, new_end_byte],
            },
        },
    );

    // scan_done_block:
    let final_start = new_start_byte;
    let final_end = new_end_byte;
    let slice_byte_len = body.add_op(
        scan_done_block,
        Operator::I32Sub,
        &[final_end, final_start],
        &[Type::I32],
    );
    let slice_scalar_len = body.add_op(
        scan_done_block,
        Operator::I32Sub,
        &[norm_end, norm_start],
        &[Type::I32],
    );
    let slice_ptr = body.add_op(
        scan_done_block,
        Operator::I32Add,
        &[ptr, final_start],
        &[Type::I32],
    );

    let new_desc = StringDescriptor {
        data_ptr: slice_ptr,
        byte_len: slice_byte_len,
        scalar_len: slice_scalar_len,
    }
    .allocate(&mut body, scan_done_block, memory, cabi_realloc);
    body.set_terminator(
        scan_done_block,
        Terminator::Return {
            values: vec![new_desc],
        },
    );

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_slice".into(), body)))
}
