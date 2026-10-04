//! Bounded materialization over the shared native read transfer.

use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type, ValueDef,
};

use crate::waffle_backend::allocation::AllocationFuncs;

/// Return (limit-exceeded, data, length). The caller owns the stream and its
/// separate capability completion. No result bytes are exposed on overflow.
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    read_transfer: Func,
) -> Result<Func> {
    let signature = module.signatures.push(SignatureData {
        params: vec![Type::I32; 2],
        returns: vec![Type::I32; 3],
    });
    let mut body = FunctionBody::new(module, signature);
    let entry = body.entry;
    let stream = body.blocks[entry].params[0].1;
    let limit = body.blocks[entry].params[1].1;
    let [zero, one, two, batch, high_bit] = [0, 1, 2, 8192, 0x8000_0000]
        .map(|value| body.add_op(entry, Operator::I32Const { value }, &[], &[Type::I32]));
    let frame = body.add_op(
        entry,
        Operator::Call {
            function_index: allocator.frame_new,
        },
        &[two],
        &[Type::I32],
    );
    let probe = body.add_op(
        entry,
        Operator::Call {
            function_index: allocator.realloc,
        },
        &[zero, zero, one, one],
        &[Type::I32],
    );
    body.add_op(
        entry,
        Operator::I32Store {
            memory: MemoryArg {
                memory,
                align: 2,
                offset: 16,
            },
        },
        &[frame, probe],
        &[],
    );
    let small = body.add_op(entry, Operator::I32LtU, &[limit, batch], &[Type::I32]);
    let capacity = body.add_op(
        entry,
        Operator::Select,
        &[limit, batch, small],
        &[Type::I32],
    );
    let empty = body.add_op(entry, Operator::I32Eqz, &[capacity], &[Type::I32]);
    let capacity = body.add_op(
        entry,
        Operator::Select,
        &[one, capacity, empty],
        &[Type::I32],
    );
    let data = body.add_op(
        entry,
        Operator::Call {
            function_index: allocator.realloc,
        },
        &[zero, zero, one, capacity],
        &[Type::I32],
    );

    let next = body.add_block();
    let data = {
        body.set_terminator(
            entry,
            Terminator::Br {
                target: BlockTarget {
                    block: next,
                    args: vec![data, capacity, zero],
                },
            },
        );
        body.add_blockparam(next, Type::I32)
    };
    let capacity = body.add_blockparam(next, Type::I32);
    let length = body.add_blockparam(next, Type::I32);
    body.add_op(
        next,
        Operator::I32Store {
            memory: MemoryArg {
                memory,
                align: 2,
                offset: 12,
            },
        },
        &[frame, data],
        &[],
    );
    let at_limit = body.add_op(next, Operator::I32Eq, &[length, limit], &[Type::I32]);
    let check_eof = body.add_block();
    let check_capacity = body.add_block();
    body.set_terminator(
        next,
        Terminator::CondBr {
            cond: at_limit,
            if_true: BlockTarget {
                block: check_eof,
                args: vec![],
            },
            if_false: BlockTarget {
                block: check_capacity,
                args: vec![],
            },
        },
    );

    let finished = body.add_block();
    let error = body.add_blockparam(finished, Type::I32);
    let final_data = body.add_blockparam(finished, Type::I32);
    let final_capacity = body.add_blockparam(finished, Type::I32);
    let final_length = body.add_blockparam(finished, Type::I32);

    // A full buffer is not evidence of EOF. A one-byte probe distinguishes an
    // exact-size body from overflow without allocating beyond the stated limit.
    let probe_call = body.add_op(
        check_eof,
        Operator::Call {
            function_index: read_transfer,
        },
        &[stream, probe, one],
        &[Type::I32; 2],
    );
    let count = body.add_value(ValueDef::PickOutput(probe_call, 0, Type::I32));
    body.append_to_block(check_eof, count);
    body.set_terminator(
        check_eof,
        Terminator::Br {
            target: BlockTarget {
                block: finished,
                args: vec![count, data, capacity, length],
            },
        },
    );

    let full = body.add_op(
        check_capacity,
        Operator::I32Eq,
        &[length, capacity],
        &[Type::I32],
    );
    let grow = body.add_block();
    let read = body.add_block();
    body.set_terminator(
        check_capacity,
        Terminator::CondBr {
            cond: full,
            if_true: BlockTarget {
                block: grow,
                args: vec![],
            },
            if_false: BlockTarget {
                block: read,
                args: vec![data, capacity],
            },
        },
    );
    let doubled = body.add_op(grow, Operator::I32Mul, &[capacity, two], &[Type::I32]);
    let overflow = body.add_op(grow, Operator::I32GeU, &[capacity, high_bit], &[Type::I32]);
    let above_limit = body.add_op(grow, Operator::I32GtU, &[doubled, limit], &[Type::I32]);
    let clamp = body.add_op(
        grow,
        Operator::I32Or,
        &[overflow, above_limit],
        &[Type::I32],
    );
    let larger = body.add_op(
        grow,
        Operator::Select,
        &[limit, doubled, clamp],
        &[Type::I32],
    );
    let grown = body.add_op(
        grow,
        Operator::Call {
            function_index: allocator.realloc,
        },
        &[data, capacity, one, larger],
        &[Type::I32],
    );
    body.set_terminator(
        grow,
        Terminator::Br {
            target: BlockTarget {
                block: read,
                args: vec![grown, larger],
            },
        },
    );

    let data = body.add_blockparam(read, Type::I32);
    let capacity = body.add_blockparam(read, Type::I32);
    body.add_op(
        read,
        Operator::I32Store {
            memory: MemoryArg {
                memory,
                align: 2,
                offset: 12,
            },
        },
        &[frame, data],
        &[],
    );
    let remaining = body.add_op(read, Operator::I32Sub, &[capacity, length], &[Type::I32]);
    let small = body.add_op(read, Operator::I32LtU, &[remaining, batch], &[Type::I32]);
    let requested = body.add_op(
        read,
        Operator::Select,
        &[remaining, batch, small],
        &[Type::I32],
    );
    let destination = body.add_op(read, Operator::I32Add, &[data, length], &[Type::I32]);
    let call = body.add_op(
        read,
        Operator::Call {
            function_index: read_transfer,
        },
        &[stream, destination, requested],
        &[Type::I32; 2],
    );
    let count = body.add_value(ValueDef::PickOutput(call, 0, Type::I32));
    body.append_to_block(read, count);
    let closed = body.add_value(ValueDef::PickOutput(call, 1, Type::I32));
    body.append_to_block(read, closed);
    let length = body.add_op(read, Operator::I32Add, &[length, count], &[Type::I32]);
    body.set_terminator(
        read,
        Terminator::CondBr {
            cond: closed,
            if_true: BlockTarget {
                block: finished,
                args: vec![zero, data, capacity, length],
            },
            if_false: BlockTarget {
                block: next,
                args: vec![data, capacity, length],
            },
        },
    );

    let success = body.add_block();
    let failure = body.add_block();
    body.set_terminator(
        finished,
        Terminator::CondBr {
            cond: error,
            if_true: BlockTarget {
                block: failure,
                args: vec![],
            },
            if_false: BlockTarget {
                block: success,
                args: vec![],
            },
        },
    );
    body.add_op(
        failure,
        Operator::Call {
            function_index: allocator.frame_drop,
        },
        &[frame],
        &[],
    );
    body.set_terminator(
        failure,
        Terminator::Return {
            values: vec![one, zero, zero],
        },
    );
    let empty = body.add_op(success, Operator::I32Eqz, &[final_length], &[Type::I32]);
    let size = body.add_op(
        success,
        Operator::Select,
        &[one, final_length, empty],
        &[Type::I32],
    );
    let data = body.add_op(
        success,
        Operator::Call {
            function_index: allocator.realloc,
        },
        &[final_data, final_capacity, one, size],
        &[Type::I32],
    );
    body.add_op(
        success,
        Operator::Call {
            function_index: allocator.frame_drop,
        },
        &[frame],
        &[],
    );
    body.set_terminator(
        success,
        Terminator::Return {
            values: vec![zero, data, final_length],
        },
    );
    body.validate()?;
    body.verify_reducible()?;
    Ok(module.funcs.push(FuncDecl::Body(
        signature,
        "stream.read-buffered".into(),
        body,
    )))
}
