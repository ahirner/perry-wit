//! UTF-8 search with scalar input and output positions.

use super::positions::{PositionMode, bounded_position};
use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type,
};

pub(super) fn emit_index_of(module: &mut Module<'static>, memory: Memory) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::F64],
        returns: vec![Type::F64],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let search = body.blocks[entry].params[1].1;
    let pos_f64 = body.blocks[entry].params[2].1;

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

    let s_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[search],
        &[Type::I32],
    );
    let s_byte_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[search],
        &[Type::I32],
    );

    let norm_pos = bounded_position(&mut body, entry, pos_f64, scalar_len, PositionMode::Clamped);
    let neg_one_f64 = body.add_op(
        entry,
        Operator::F64Const {
            value: (-1f64).to_bits(),
        },
        &[],
        &[Type::F64],
    );
    let zero_i32 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let one = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let c0 = body.add_op(entry, Operator::I32Const { value: 0xC0 }, &[], &[Type::I32]);
    let c80 = body.add_op(entry, Operator::I32Const { value: 0x80 }, &[], &[Type::I32]);

    // If s_byte_len == 0:
    let search_is_empty = body.add_op(entry, Operator::I32Eqz, &[s_byte_len], &[Type::I32]);
    let empty_search_block = body.add_block();
    body.blocks[empty_search_block].desc = "index_of empty search".into();
    let normal_search_block = body.add_block();
    body.blocks[normal_search_block].desc = "index_of normal search".into();

    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: search_is_empty,
            if_true: BlockTarget {
                block: empty_search_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: normal_search_block,
                args: vec![],
            },
        },
    );

    // empty_search_block: min(norm_pos, scalar_len) as f64
    let empty_exceeds = body.add_op(
        empty_search_block,
        Operator::I32GtS,
        &[norm_pos, scalar_len],
        &[Type::I32],
    );
    let empty_pos_clamped = body.add_op(
        empty_search_block,
        Operator::Select,
        &[scalar_len, norm_pos, empty_exceeds],
        &[Type::I32],
    );
    let empty_res_f64 = body.add_op(
        empty_search_block,
        Operator::F64ConvertI32U,
        &[empty_pos_clamped],
        &[Type::F64],
    );
    body.set_terminator(
        empty_search_block,
        Terminator::Return {
            values: vec![empty_res_f64],
        },
    );

    // normal_search_block: if norm_pos >= scalar_len: return -1.0
    let pos_ge_scalar = body.add_op(
        normal_search_block,
        Operator::I32GeS,
        &[norm_pos, scalar_len],
        &[Type::I32],
    );
    let out_of_range_block = body.add_block();
    let find_byte_pos_block = body.add_block();

    body.set_terminator(
        normal_search_block,
        Terminator::CondBr {
            cond: pos_ge_scalar,
            if_true: BlockTarget {
                block: out_of_range_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: find_byte_pos_block,
                args: vec![],
            },
        },
    );

    body.set_terminator(
        out_of_range_block,
        Terminator::Return {
            values: vec![neg_one_f64],
        },
    );

    // Loop to find start_byte corresponding to norm_pos
    let find_pos_loop = body.add_block();
    body.blocks[find_pos_loop].desc = "find_start_byte loop".into();
    let f_byte_idx = body.add_blockparam(find_pos_loop, Type::I32);
    let f_scalar_idx = body.add_blockparam(find_pos_loop, Type::I32);

    let search_loop_init = body.add_block();
    body.blocks[search_loop_init].desc = "search_loop init".into();
    let found_start_byte = body.add_blockparam(search_loop_init, Type::I32);

    body.set_terminator(
        find_byte_pos_block,
        Terminator::Br {
            target: BlockTarget {
                block: find_pos_loop,
                args: vec![zero_i32, zero_i32],
            },
        },
    );

    let reached_norm_pos = body.add_op(
        find_pos_loop,
        Operator::I32GeS,
        &[f_scalar_idx, norm_pos],
        &[Type::I32],
    );
    let f_step = body.add_block();

    body.set_terminator(
        find_pos_loop,
        Terminator::CondBr {
            cond: reached_norm_pos,
            if_true: BlockTarget {
                block: search_loop_init,
                args: vec![f_byte_idx],
            },
            if_false: BlockTarget {
                block: f_step,
                args: vec![],
            },
        },
    );

    let f_b_addr = body.add_op(f_step, Operator::I32Add, &[ptr, f_byte_idx], &[Type::I32]);
    let f_b = body.add_op(
        f_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[f_b_addr],
        &[Type::I32],
    );
    let f_band = body.add_op(f_step, Operator::I32And, &[f_b, c0], &[Type::I32]);
    let f_is_lead = body.add_op(f_step, Operator::I32Ne, &[f_band, c80], &[Type::I32]);
    let next_f_sc = body.add_op(
        f_step,
        Operator::I32Add,
        &[f_scalar_idx, f_is_lead],
        &[Type::I32],
    );
    let next_f_bi = body.add_op(f_step, Operator::I32Add, &[f_byte_idx, one], &[Type::I32]);

    body.set_terminator(
        f_step,
        Terminator::Br {
            target: BlockTarget {
                block: find_pos_loop,
                args: vec![next_f_bi, next_f_sc],
            },
        },
    );

    // Search outer loop: candidate byte index from found_start_byte up to byte_len - s_byte_len
    let outer_loop = body.add_block();
    body.blocks[outer_loop].desc = "index_of outer loop".into();
    let cand_byte = body.add_blockparam(outer_loop, Type::I32);
    let not_found_block = body.add_block();

    body.set_terminator(
        search_loop_init,
        Terminator::Br {
            target: BlockTarget {
                block: outer_loop,
                args: vec![found_start_byte],
            },
        },
    );

    // max_cand = byte_len - s_byte_len
    let max_cand = body.add_op(
        outer_loop,
        Operator::I32Sub,
        &[byte_len, s_byte_len],
        &[Type::I32],
    );
    let cand_overflow = body.add_op(
        outer_loop,
        Operator::I32GtS,
        &[cand_byte, max_cand],
        &[Type::I32],
    );
    let match_check_block = body.add_block();

    body.set_terminator(
        outer_loop,
        Terminator::CondBr {
            cond: cand_overflow,
            if_true: BlockTarget {
                block: not_found_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: match_check_block,
                args: vec![],
            },
        },
    );

    body.set_terminator(
        not_found_block,
        Terminator::Return {
            values: vec![neg_one_f64],
        },
    );

    // Inner loop: check if search bytes match candidate
    let inner_loop = body.add_block();
    body.blocks[inner_loop].desc = "index_of inner cmp loop".into();
    let inner_idx = body.add_blockparam(inner_loop, Type::I32);

    let match_success_block = body.add_block();
    body.blocks[match_success_block].desc = "index_of match success".into();

    body.set_terminator(
        match_check_block,
        Terminator::Br {
            target: BlockTarget {
                block: inner_loop,
                args: vec![zero_i32],
            },
        },
    );

    let inner_done = body.add_op(
        inner_loop,
        Operator::I32GeU,
        &[inner_idx, s_byte_len],
        &[Type::I32],
    );
    let inner_step = body.add_block();

    body.set_terminator(
        inner_loop,
        Terminator::CondBr {
            cond: inner_done,
            if_true: BlockTarget {
                block: match_success_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: inner_step,
                args: vec![],
            },
        },
    );

    let h_addr = body.add_op(
        inner_step,
        Operator::I32Add,
        &[ptr, cand_byte],
        &[Type::I32],
    );
    let h_char_addr = body.add_op(
        inner_step,
        Operator::I32Add,
        &[h_addr, inner_idx],
        &[Type::I32],
    );
    let h_b = body.add_op(
        inner_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[h_char_addr],
        &[Type::I32],
    );

    let n_char_addr = body.add_op(
        inner_step,
        Operator::I32Add,
        &[s_ptr, inner_idx],
        &[Type::I32],
    );
    let n_b = body.add_op(
        inner_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[n_char_addr],
        &[Type::I32],
    );

    let chars_match = body.add_op(inner_step, Operator::I32Eq, &[h_b, n_b], &[Type::I32]);
    let inner_next_block = body.add_block();
    let outer_next_block = body.add_block();

    body.set_terminator(
        inner_step,
        Terminator::CondBr {
            cond: chars_match,
            if_true: BlockTarget {
                block: inner_next_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: outer_next_block,
                args: vec![],
            },
        },
    );

    let next_inner_idx = body.add_op(
        inner_next_block,
        Operator::I32Add,
        &[inner_idx, one],
        &[Type::I32],
    );
    body.set_terminator(
        inner_next_block,
        Terminator::Br {
            target: BlockTarget {
                block: inner_loop,
                args: vec![next_inner_idx],
            },
        },
    );

    let next_cand_byte = body.add_op(
        outer_next_block,
        Operator::I32Add,
        &[cand_byte, one],
        &[Type::I32],
    );
    body.set_terminator(
        outer_next_block,
        Terminator::Br {
            target: BlockTarget {
                block: outer_loop,
                args: vec![next_cand_byte],
            },
        },
    );

    // match_success_block: count scalars from 0 up to cand_byte
    let count_loop = body.add_block();
    body.blocks[count_loop].desc = "index_of count scalars".into();
    let c_b_idx = body.add_blockparam(count_loop, Type::I32);
    let c_sc_cnt = body.add_blockparam(count_loop, Type::I32);

    let return_match = body.add_block();
    let ret_sc = body.add_blockparam(return_match, Type::I32);

    body.set_terminator(
        match_success_block,
        Terminator::Br {
            target: BlockTarget {
                block: count_loop,
                args: vec![zero_i32, zero_i32],
            },
        },
    );

    let c_done = body.add_op(
        count_loop,
        Operator::I32GeU,
        &[c_b_idx, cand_byte],
        &[Type::I32],
    );
    let c_step = body.add_block();

    body.set_terminator(
        count_loop,
        Terminator::CondBr {
            cond: c_done,
            if_true: BlockTarget {
                block: return_match,
                args: vec![c_sc_cnt],
            },
            if_false: BlockTarget {
                block: c_step,
                args: vec![],
            },
        },
    );

    let c_addr = body.add_op(c_step, Operator::I32Add, &[ptr, c_b_idx], &[Type::I32]);
    let c_b = body.add_op(
        c_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[c_addr],
        &[Type::I32],
    );
    let c_band = body.add_op(c_step, Operator::I32And, &[c_b, c0], &[Type::I32]);
    let c_is_lead = body.add_op(c_step, Operator::I32Ne, &[c_band, c80], &[Type::I32]);
    let next_c_sc = body.add_op(
        c_step,
        Operator::I32Add,
        &[c_sc_cnt, c_is_lead],
        &[Type::I32],
    );
    let next_c_bi = body.add_op(c_step, Operator::I32Add, &[c_b_idx, one], &[Type::I32]);

    body.set_terminator(
        c_step,
        Terminator::Br {
            target: BlockTarget {
                block: count_loop,
                args: vec![next_c_bi, next_c_sc],
            },
        },
    );

    let final_match_f64 = body.add_op(
        return_match,
        Operator::F64ConvertI32U,
        &[ret_sc],
        &[Type::F64],
    );
    body.set_terminator(
        return_match,
        Terminator::Return {
            values: vec![final_match_f64],
        },
    );

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_index_of".into(), body)))
}
