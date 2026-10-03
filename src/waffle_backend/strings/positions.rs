//! Numeric normalization shared by scalar string operations.

use anyhow::Result;
use waffle::{
    Block, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData,
    Terminator, Type, Value,
};

pub(super) enum PositionMode {
    Relative,
    Clamped,
}

/// Implements ToIntegerOrInfinity while still in f64, mapping NaN to zero.
pub(super) fn integer_position(body: &mut FunctionBody, block: Block, value: Value) -> Value {
    let zero = body.add_op(block, Operator::F64Const { value: 0 }, &[], &[Type::F64]);
    let integer = body.add_op(block, Operator::F64Trunc, &[value], &[Type::F64]);
    let is_nan = body.add_op(block, Operator::F64Ne, &[value, value], &[Type::I32]);
    body.add_op(
        block,
        Operator::Select,
        &[zero, integer, is_nan],
        &[Type::F64],
    )
}

/// Applies relative or absolute bounds before narrowing a position to a memory-sized integer.
pub(super) fn bounded_position(
    body: &mut FunctionBody,
    block: Block,
    value: Value,
    length: Value,
    mode: PositionMode,
) -> Value {
    let integer = integer_position(body, block, value);
    let zero = body.add_op(block, Operator::F64Const { value: 0 }, &[], &[Type::F64]);
    let length = body.add_op(block, Operator::F64ConvertI32U, &[length], &[Type::F64]);
    let position = match mode {
        PositionMode::Clamped => integer,
        PositionMode::Relative => {
            let negative = body.add_op(block, Operator::F64Lt, &[integer, zero], &[Type::I32]);
            let relative = body.add_op(block, Operator::F64Add, &[length, integer], &[Type::F64]);
            body.add_op(
                block,
                Operator::Select,
                &[relative, integer, negative],
                &[Type::F64],
            )
        }
    };
    let nonnegative = body.add_op(block, Operator::F64Max, &[position, zero], &[Type::F64]);
    let bounded = body.add_op(
        block,
        Operator::F64Min,
        &[nonnegative, length],
        &[Type::F64],
    );
    body.add_op(block, Operator::I32TruncF64U, &[bounded], &[Type::I32])
}

/// Character access checks absolute bounds before invoking the relative slice operation.
pub(super) fn emit_char_at(
    module: &mut Module<'static>,
    memory: Memory,
    slice: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::F64],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let index = body.blocks[entry].params[1].1;
    let index = integer_position(&mut body, entry, index);
    let length = body.add_op(
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
    let length = body.add_op(entry, Operator::F64ConvertI32U, &[length], &[Type::F64]);
    let zero = body.add_op(entry, Operator::F64Const { value: 0 }, &[], &[Type::F64]);
    let nonnegative = body.add_op(entry, Operator::F64Ge, &[index, zero], &[Type::I32]);
    let in_range = body.add_op(entry, Operator::F64Lt, &[index, length], &[Type::I32]);
    let valid = body.add_op(
        entry,
        Operator::I32And,
        &[nonnegative, in_range],
        &[Type::I32],
    );
    let one = body.add_op(
        entry,
        Operator::F64Const {
            value: 1f64.to_bits(),
        },
        &[],
        &[Type::F64],
    );
    let end = body.add_op(entry, Operator::F64Add, &[index, one], &[Type::F64]);
    let start = body.add_op(entry, Operator::Select, &[index, zero, valid], &[Type::F64]);
    let end = body.add_op(entry, Operator::Select, &[end, zero, valid], &[Type::F64]);
    let result = body.add_op(
        entry,
        Operator::Call {
            function_index: slice,
        },
        &[desc, start, end],
        &[Type::I32],
    );
    body.set_terminator(
        entry,
        Terminator::Return {
            values: vec![result],
        },
    );
    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_char_at".into(), body)))
}
