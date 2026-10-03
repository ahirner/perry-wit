//! Numeric normalization shared by scalar string operations.

use anyhow::Result;
use waffle::{
    Block, BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator,
    SignatureData, Terminator, Type, Value,
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

pub(super) enum CharacterAccess {
    CharAt,
    Index,
}

/// Bracket access uses integral property indices and a null descriptor for undefined.
/// charAt truncates positions and returns an allocated empty string outside the bounds.
pub(super) fn emit_character_access(
    module: &mut Module<'static>,
    memory: Memory,
    slice: Func,
    access: CharacterAccess,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::F64],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let position = body.blocks[entry].params[1].1;
    let index = integer_position(&mut body, entry, position);
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
    let mut valid = body.add_op(
        entry,
        Operator::I32And,
        &[nonnegative, in_range],
        &[Type::I32],
    );
    if matches!(access, CharacterAccess::Index) {
        let integral = body.add_op(entry, Operator::F64Eq, &[position, index], &[Type::I32]);
        valid = body.add_op(entry, Operator::I32And, &[valid, integral], &[Type::I32]);
    }
    let found = body.add_block();
    let missing = body.add_block();
    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: valid,
            if_true: BlockTarget {
                block: found,
                args: vec![],
            },
            if_false: BlockTarget {
                block: missing,
                args: vec![],
            },
        },
    );
    let missing_result = match access {
        CharacterAccess::Index => {
            body.add_op(missing, Operator::I32Const { value: 0 }, &[], &[Type::I32])
        }
        CharacterAccess::CharAt => body.add_op(
            missing,
            Operator::Call {
                function_index: slice,
            },
            &[desc, zero, zero],
            &[Type::I32],
        ),
    };
    body.set_terminator(
        missing,
        Terminator::Return {
            values: vec![missing_result],
        },
    );
    let one = body.add_op(
        found,
        Operator::F64Const {
            value: 1f64.to_bits(),
        },
        &[],
        &[Type::F64],
    );
    let end = body.add_op(found, Operator::F64Add, &[index, one], &[Type::F64]);
    let result = body.add_op(
        found,
        Operator::Call {
            function_index: slice,
        },
        &[desc, index, end],
        &[Type::I32],
    );
    body.set_terminator(
        found,
        Terminator::Return {
            values: vec![result],
        },
    );
    body.validate()?;
    body.verify_reducible()?;
    let name = match access {
        CharacterAccess::CharAt => "$rt_str_char_at",
        CharacterAccess::Index => "$rt_str_index",
    };
    Ok(module.funcs.push(FuncDecl::Body(sig, name.into(), body)))
}
