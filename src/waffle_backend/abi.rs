//! Call and Exit ABI handling for WAFFLE backend.
//!
//! Encapsulates:
//! - Canonical payload encoding and decoding between language values and f64 payloads.
//! - Exception-aware guest completions and host-facing export wrappers.
//! - Internal call invocations and branch outcome dispatch.

use anyhow::Result;
use waffle::{
    Block, BlockTarget, FunctionBody, Module, Operator, Terminator, Type, Value, ValueDef,
};

use crate::waffle_backend::registry::{
    ExportConvention, FunctionExport, FunctionInfo, PrimitivePayload,
};

/// Completion tags shared by guest calls and WIT result discriminants.
#[derive(Clone, Copy)]
pub(crate) enum CompletionStatus {
    Returned = 0,
    Threw = 1,
}

/// Converts an exception-aware guest completion at the host boundary only.
pub(crate) fn build_export_wrapper(
    module: &Module<'static>,
    callee: &FunctionInfo,
    export: &FunctionExport,
    memory: waffle::Memory,
) -> Result<FunctionBody> {
    let mut body = FunctionBody::new(module, export.sig);
    let entry = body.entry;
    let args: Vec<_> = body.blocks[entry]
        .params
        .iter()
        .map(|&(_, value)| value)
        .collect();
    let outcome = emit_internal_call(&mut body, entry, callee, &args);
    match export.convention {
        ExportConvention::Direct => {
            let values = module.signatures[export.sig]
                .returns
                .iter()
                .map(|&ty| {
                    decode_payload(
                        &mut body,
                        outcome.ok_block,
                        outcome.payload,
                        ty == Type::I32,
                    )
                })
                .collect();
            body.set_terminator(outcome.ok_block, Terminator::Return { values });
            body.set_terminator(outcome.err_block, Terminator::Unreachable);
        }
        ExportConvention::WitResult { success } => {
            for (block, status, ty) in [
                (outcome.ok_block, CompletionStatus::Returned, success),
                (
                    outcome.err_block,
                    CompletionStatus::Threw,
                    PrimitivePayload::Number,
                ),
            ] {
                let retptr =
                    emit_retptr_store(&mut body, block, memory, status, outcome.payload, ty);
                body.set_terminator(
                    block,
                    Terminator::Return {
                        values: vec![retptr],
                    },
                );
            }
        }
    }
    body.validate()?;
    body.verify_reducible()?;
    Ok(body)
}

/// Returns a language completion to a guest caller without host conversion.
pub(crate) fn emit_completion(
    body: &mut FunctionBody,
    block: Block,
    status: CompletionStatus,
    payload: Value,
) {
    let status = body.add_op(
        block,
        Operator::I32Const {
            value: status as u32,
        },
        &[],
        &[Type::I32],
    );
    body.set_terminator(
        block,
        Terminator::Return {
            values: vec![status, payload],
        },
    );
}

/// Canonical payload encoding: converts an optional WAFFLE value into a single f64 payload Value.
pub(crate) fn encode_payload(body: &mut FunctionBody, block: Block, val: Option<Value>) -> Value {
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

/// Stores a WIT Result tag and its declared primitive payload at the return pointer.
/// Numeric error payloads align the union to eight bytes, including boolean success variants.
fn emit_retptr_store(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    status: CompletionStatus,
    payload_f64: Value,
    payload_type: PrimitivePayload,
) -> Value {
    let addr = body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
    let status_val = body.add_op(
        block,
        Operator::I32Const {
            value: status as u32,
        },
        &[],
        &[Type::I32],
    );

    body.add_op(
        block,
        Operator::I32Store8 {
            memory: waffle::MemoryArg {
                align: 0,
                offset: 0,
                memory,
            },
        },
        &[addr, status_val],
        &[],
    );
    let (payload, store) = match payload_type {
        PrimitivePayload::Number => (
            payload_f64,
            Operator::F64Store {
                memory: waffle::MemoryArg {
                    align: 3,
                    offset: 8,
                    memory,
                },
            },
        ),
        PrimitivePayload::Boolean => (
            decode_payload(body, block, payload_f64, true),
            Operator::I32Store8 {
                memory: waffle::MemoryArg {
                    align: 0,
                    offset: 8,
                    memory,
                },
            },
        ),
    };
    body.add_op(block, store, &[addr, payload], &[]);

    addr
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
