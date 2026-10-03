//! Scalar string ordering with an explicit undefined sentinel for equality.

use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type,
};

/// Null descriptors compare equal only to each other; callers reject ordering them.
pub(super) fn emit_compare(module: &mut Module<'static>, memory: Memory) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let a = body.blocks[entry].params[0].1;
    let b = body.blocks[entry].params[1].1;

    let a_missing = body.add_op(entry, Operator::I32Eqz, &[a], &[Type::I32]);
    let b_missing = body.add_op(entry, Operator::I32Eqz, &[b], &[Type::I32]);
    let either_missing = body.add_op(
        entry,
        Operator::I32Or,
        &[a_missing, b_missing],
        &[Type::I32],
    );
    let missing = body.add_block();
    let present = body.add_block();
    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: either_missing,
            if_true: BlockTarget {
                block: missing,
                args: vec![],
            },
            if_false: BlockTarget {
                block: present,
                args: vec![],
            },
        },
    );
    let different = body.add_op(missing, Operator::I32Ne, &[a, b], &[Type::I32]);
    body.set_terminator(
        missing,
        Terminator::Return {
            values: vec![different],
        },
    );
    let entry = present;

    let a_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[a],
        &[Type::I32],
    );
    let a_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[a],
        &[Type::I32],
    );

    let b_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[b],
        &[Type::I32],
    );
    let b_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[b],
        &[Type::I32],
    );

    // min_len = min(a_len, b_len)
    let a_lt_b_len = body.add_op(entry, Operator::I32LtU, &[a_len, b_len], &[Type::I32]);
    let min_len = body.add_op(
        entry,
        Operator::Select,
        &[a_len, b_len, a_lt_b_len],
        &[Type::I32],
    );

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let one = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let neg_one = body.add_op(
        entry,
        Operator::I32Const {
            value: (-1i32) as u32,
        },
        &[],
        &[Type::I32],
    );

    let cmp_loop = body.add_block();
    let idx = body.add_blockparam(cmp_loop, Type::I32);
    body.set_terminator(
        entry,
        Terminator::Br {
            target: BlockTarget {
                block: cmp_loop,
                args: vec![zero],
            },
        },
    );

    let loop_done = body.add_op(cmp_loop, Operator::I32GeU, &[idx, min_len], &[Type::I32]);
    let check_len_block = body.add_block();
    let cmp_step = body.add_block();

    body.set_terminator(
        cmp_loop,
        Terminator::CondBr {
            cond: loop_done,
            if_true: BlockTarget {
                block: check_len_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: cmp_step,
                args: vec![],
            },
        },
    );

    let a_c_addr = body.add_op(cmp_step, Operator::I32Add, &[a_ptr, idx], &[Type::I32]);
    let a_byte = body.add_op(
        cmp_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[a_c_addr],
        &[Type::I32],
    );

    let b_c_addr = body.add_op(cmp_step, Operator::I32Add, &[b_ptr, idx], &[Type::I32]);
    let b_byte = body.add_op(
        cmp_step,
        Operator::I32Load8U {
            memory: MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[b_c_addr],
        &[Type::I32],
    );

    let a_lt_b = body.add_op(cmp_step, Operator::I32LtU, &[a_byte, b_byte], &[Type::I32]);
    let a_gt_b = body.add_op(cmp_step, Operator::I32GtU, &[a_byte, b_byte], &[Type::I32]);

    let ret_neg = body.add_block();
    let check_gt = body.add_block();

    body.set_terminator(
        cmp_step,
        Terminator::CondBr {
            cond: a_lt_b,
            if_true: BlockTarget {
                block: ret_neg,
                args: vec![],
            },
            if_false: BlockTarget {
                block: check_gt,
                args: vec![],
            },
        },
    );

    body.set_terminator(
        ret_neg,
        Terminator::Return {
            values: vec![neg_one],
        },
    );

    let ret_pos = body.add_block();
    let loop_cont = body.add_block();

    body.set_terminator(
        check_gt,
        Terminator::CondBr {
            cond: a_gt_b,
            if_true: BlockTarget {
                block: ret_pos,
                args: vec![],
            },
            if_false: BlockTarget {
                block: loop_cont,
                args: vec![],
            },
        },
    );

    body.set_terminator(ret_pos, Terminator::Return { values: vec![one] });

    let next_idx = body.add_op(loop_cont, Operator::I32Add, &[idx, one], &[Type::I32]);
    body.set_terminator(
        loop_cont,
        Terminator::Br {
            target: BlockTarget {
                block: cmp_loop,
                args: vec![next_idx],
            },
        },
    );

    // check_len_block:
    let len_lt = body.add_op(
        check_len_block,
        Operator::I32LtU,
        &[a_len, b_len],
        &[Type::I32],
    );
    let len_gt = body.add_op(
        check_len_block,
        Operator::I32GtU,
        &[a_len, b_len],
        &[Type::I32],
    );
    let len_gt_res = body.add_op(
        check_len_block,
        Operator::Select,
        &[one, zero, len_gt],
        &[Type::I32],
    );
    let len_res = body.add_op(
        check_len_block,
        Operator::Select,
        &[neg_one, len_gt_res, len_lt],
        &[Type::I32],
    );
    body.set_terminator(
        check_len_block,
        Terminator::Return {
            values: vec![len_res],
        },
    );

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_compare".into(), body)))
}
