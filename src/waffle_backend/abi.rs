//! Call and Exit ABI handling for WAFFLE backend.
//!
//! Encapsulates:
//! - Canonical payload encoding and decoding between language values and f64 payloads.
//! - Function return and throw exits across calling conventions (Internal, ExportedDirect, ExportedWitResult).
//! - Internal call invocations and branch outcome dispatch.

use waffle::{Block, BlockTarget, FunctionBody, Operator, Terminator, Type, Value, ValueDef};

use crate::waffle_backend::registry::{CallingConvention, FunctionInfo};

/// Canonical payload encoding: converts an optional WAFFLE value into a single f64 payload Value.
pub(crate) fn encode_payload(
    body: &mut FunctionBody,
    block: Block,
    val: Option<Value>,
) -> Value {
    if let Some(v) = val {
        let ty = body.values[v].ty(&body.type_pool).unwrap_or(Type::F64);
        if ty == Type::I32 {
            body.add_op(block, Operator::F64ConvertI32U, &[v], &[Type::F64])
        } else {
            v
        }
    } else {
        body.add_op(
            block,
            Operator::F64Const {
                value: 0f64.to_bits(),
            },
            &[],
            &[Type::F64],
        )
    }
}

/// Canonical payload decoding: converts an f64 payload into the expected target WAFFLE representation.
pub(crate) fn decode_payload(
    body: &mut FunctionBody,
    block: Block,
    payload: Value,
    is_boolean: bool,
) -> Value {
    if is_boolean {
        body.add_op(block, Operator::I32TruncF64U, &[payload], &[Type::I32])
    } else {
        payload
    }
}

/// Stores a WIT Result discriminant (0=Ok, 1=Err) and f64 payload into linear memory retptr.
/// Returns the retptr address Value (i32: 8).
pub(crate) fn emit_retptr_store(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    status: u32,
    payload_f64: Value,
) -> Value {
    let addr = body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
    let status_val = body.add_op(block, Operator::I32Const { value: status }, &[], &[Type::I32]);

    body.add_op(
        block,
        Operator::I32Store {
            memory: waffle::MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[addr, status_val],
        &[],
    );
    body.add_op(
        block,
        Operator::F64Store {
            memory: waffle::MemoryArg {
                align: 3,
                offset: 8,
                memory,
            },
        },
        &[addr, payload_f64],
        &[],
    );

    addr
}

/// Emits the terminal return exit for a function based on its calling convention.
pub(crate) fn emit_function_return(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    convention: CallingConvention,
    expected_returns: &[Type],
    ret_val: Option<Value>,
) {
    match convention {
        CallingConvention::ExportedWitResult => {
            let payload_f64 = encode_payload(body, block, ret_val);
            let retptr = emit_retptr_store(body, block, memory, 0, payload_f64);
            body.set_terminator(block, Terminator::Return { values: vec![retptr] });
        }
        CallingConvention::ExportedDirect => {
            let values = if let Some(val) = ret_val {
                if expected_returns.len() == 1 {
                    let ty = body.values[val].ty(&body.type_pool).unwrap_or(Type::F64);
                    if expected_returns[0] == Type::I32 && ty == Type::F64 {
                        vec![body.add_op(block, Operator::I32TruncF64U, &[val], &[Type::I32])]
                    } else if expected_returns[0] == Type::F64 && ty == Type::I32 {
                        vec![body.add_op(block, Operator::F64ConvertI32U, &[val], &[Type::F64])]
                    } else {
                        vec![val]
                    }
                } else {
                    vec![val]
                }
            } else if expected_returns.len() == 1 && expected_returns[0] == Type::F64 {
                let zero = body.add_op(
                    block,
                    Operator::F64Const {
                        value: 0f64.to_bits(),
                    },
                    &[],
                    &[Type::F64],
                );
                vec![zero]
            } else if expected_returns.len() == 1 && expected_returns[0] == Type::I32 {
                let zero = body.add_op(block, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
                vec![zero]
            } else {
                vec![]
            };
            body.set_terminator(block, Terminator::Return { values });
        }
        CallingConvention::Internal => {
            let payload_f64 = encode_payload(body, block, ret_val);
            let ok_status = body.add_op(block, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
            body.set_terminator(
                block,
                Terminator::Return {
                    values: vec![ok_status, payload_f64],
                },
            );
        }
    }
}

/// Emits the terminal throw exit for a function based on its calling convention.
pub(crate) fn emit_function_throw(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    convention: CallingConvention,
    payload_f64: Value,
) {
    match convention {
        CallingConvention::ExportedWitResult => {
            let retptr = emit_retptr_store(body, block, memory, 1, payload_f64);
            body.set_terminator(block, Terminator::Return { values: vec![retptr] });
        }
        CallingConvention::ExportedDirect => {
            // Uncaught exception in infallible export traps at runtime
            body.set_terminator(block, Terminator::Unreachable);
        }
        CallingConvention::Internal => {
            let err_status = body.add_op(block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
            body.set_terminator(
                block,
                Terminator::Return {
                    values: vec![err_status, payload_f64],
                },
            );
        }
    }
}

/// Result of emitting an internal call branch.
pub(crate) struct InternalCallOutcome {
    pub(crate) ok_block: Block,
    pub(crate) err_block: Block,
    pub(crate) payload: Value,
}

/// Emits an internal call with `(status: i32, payload: f64)` splitting into `ok_block` and `err_block`.
pub(crate) fn emit_internal_call(
    body: &mut FunctionBody,
    current_block: Block,
    callee: &FunctionInfo,
    args: &[Value],
) -> InternalCallOutcome {
    let call_val = body.add_op(
        current_block,
        Operator::Call {
            function_index: callee.func_index,
        },
        args,
        &[Type::I32, Type::F64],
    );
    let status = body.add_value(ValueDef::PickOutput(call_val, 0, Type::I32));
    body.append_to_block(current_block, status);
    let payload = body.add_value(ValueDef::PickOutput(call_val, 1, Type::F64));
    body.append_to_block(current_block, payload);

    let is_ok = body.add_op(current_block, Operator::I32Eqz, &[status], &[Type::I32]);
    let ok_block = body.add_block();
    body.blocks[ok_block].desc = format!("call {} ok", callee.name);
    let err_block = body.add_block();
    body.blocks[err_block].desc = format!("call {} err", callee.name);

    body.set_terminator(
        current_block,
        Terminator::CondBr {
            cond: is_ok,
            if_true: BlockTarget {
                block: ok_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: err_block,
                args: vec![],
            },
        },
    );

    InternalCallOutcome {
        ok_block,
        err_block,
        payload,
    }
}
