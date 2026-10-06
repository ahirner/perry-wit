//! Call and Exit ABI handling for WAFFLE backend.
//!
//! Encapsulates:
//! - Canonical payload encoding and decoding between language values and f64 payloads.
//! - Exception-aware guest completions and host-facing export wrappers.
//! - Internal call invocations and branch outcome dispatch.

use anyhow::Result;
use waffle::{
    Block, BlockTarget, Func, FunctionBody, Module, Operator, Terminator, Type, Value, ValueDef,
};

use super::allocation::RetainedValues;
use crate::waffle_backend::registry::{
    ExportConvention, FunctionExport, FunctionInfo, ModuleRegistry, ValuePayload,
};
use perry_hir::types::Type as HirType;

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
    registry: &ModuleRegistry,
) -> Result<FunctionBody> {
    let memory = registry.memory;
    let lift_canonical = registry
        .string_helpers
        .map(|helpers| helpers.lift_canonical);
    let lift_bytes = registry.byte_helpers.map(|helpers| helpers.lift_canonical);
    let lift_text_or_bytes = registry.text_or_bytes_lift;
    let mut body = FunctionBody::new(module, export.sig);
    let entry = body.entry;
    if let Some(native) = registry.promises.as_ref().map(|runtime| &runtime.native) {
        body.add_op(
            entry,
            Operator::Call {
                function_index: native.enter,
            },
            &[],
            &[],
        );
    }
    let mut streams = Vec::new();
    let mut args = Vec::new();
    let mut param_cursor = 0;
    for param_ty in &callee.param_types {
        if super::streams::web::Kind::of(param_ty) == Some(super::streams::web::Kind::Readable) {
            let raw = body.blocks[entry].params[param_cursor].1;
            param_cursor += 1;
            let stream = body.add_op(
                entry,
                Operator::Call {
                    function_index: registry.web_streams.unwrap().input,
                },
                &[raw],
                &[Type::I32],
            );
            args.push(stream);
            streams.push(stream);
            continue;
        }
        if super::values::is_dynamic(param_ty) {
            let value = body.blocks[entry].params[param_cursor].1;
            param_cursor += 1;
            let tag = body.add_op(
                entry,
                Operator::I32Const {
                    value: super::values::ValueTag::Number as u32,
                },
                &[],
                &[Type::I32],
            );
            let boxed = body.add_op(
                entry,
                Operator::Call {
                    function_index: registry
                        .value_helpers
                        .expect("dynamic values require helpers")
                        .new,
                },
                &[tag, value],
                &[Type::I32],
            );
            args.push(boxed);
            continue;
        }
        if super::text_or_bytes::is_text_or_bytes(param_ty) || super::filesystem::is_stats(param_ty)
        {
            let values: Vec<_> = body.blocks[entry].params[param_cursor..param_cursor + 3]
                .iter()
                .map(|param| param.1)
                .collect();
            param_cursor += 3;
            let value = body.add_op(
                entry,
                Operator::Call {
                    function_index: if super::filesystem::is_stats(param_ty) {
                        registry
                            .structured_helpers
                            .expect("Stats lifting is available")
                            .lift_stats
                    } else {
                        lift_text_or_bytes.ok_or_else(|| {
                            anyhow::anyhow!("String-or-byte parameters require canonical lifting")
                        })?
                    },
                },
                &values,
                &[Type::I32],
            );
            args.push(value);
            continue;
        }
        let lift = match param_ty {
            ty if super::structured::is_string_array(ty) => Some(
                registry
                    .structured_helpers
                    .expect("String arrays require canonical lifting")
                    .lift_strings,
            ),
            HirType::String => {
                Some(lift_canonical.ok_or_else(|| {
                    anyhow::anyhow!("String parameters require canonical lifting")
                })?)
            }
            ty if super::bytes::is_byte_view(ty) => Some(
                lift_bytes
                    .ok_or_else(|| anyhow::anyhow!("Byte parameters require canonical lifting"))?,
            ),
            _ => None,
        };
        if let Some(lift_fn) = lift {
            let ptr = body.blocks[entry].params[param_cursor].1;
            let byte_len = body.blocks[entry].params[param_cursor + 1].1;
            param_cursor += 2;
            let desc = body.add_op(
                entry,
                Operator::Call {
                    function_index: lift_fn,
                },
                &[ptr, byte_len],
                &[Type::I32],
            );
            args.push(desc);
        } else {
            let val = body.blocks[entry].params[param_cursor].1;
            param_cursor += 1;
            args.push(val);
        }
    }
    let mut outcome = emit_fallible_call(&mut body, entry, callee.func_index, &args);
    let mut error_payload = outcome.payload;
    if super::values::is_dynamic(callee.success_type()) {
        let value = decode_payload(&mut body, outcome.ok_block, outcome.payload, true);
        let tag = body.add_op(
            outcome.ok_block,
            Operator::I32Const {
                value: super::values::ValueTag::Number as u32,
            },
            &[],
            &[Type::I32],
        );
        let extracted = emit_fallible_call(
            &mut body,
            outcome.ok_block,
            registry
                .value_helpers
                .expect("dynamic values require helpers")
                .extract,
            &[value, tag],
        );
        let errors = body.add_block();
        error_payload = body.add_blockparam(errors, Type::F64);
        for (block, payload) in [
            (outcome.err_block, outcome.payload),
            (extracted.err_block, extracted.payload),
        ] {
            body.set_terminator(
                block,
                Terminator::Br {
                    target: BlockTarget {
                        block: errors,
                        args: vec![payload],
                    },
                },
            );
        }
        outcome = InternalCallOutcome {
            ok_block: extracted.ok_block,
            err_block: errors,
            payload: extracted.payload,
        };
    }
    if let Some(native) = registry.promises.as_ref().map(|runtime| &runtime.native) {
        for (block, retain) in [
            (
                outcome.ok_block,
                super::ssa::types::is_reference(callee.success_type())
                    && !super::values::is_dynamic(callee.success_type()),
            ),
            (outcome.err_block, false),
        ] {
            let root = retain.then(|| {
                let value = decode_payload(&mut body, block, outcome.payload, true);
                RetainedValues::new(
                    &mut body,
                    block,
                    memory,
                    registry.allocator.unwrap(),
                    &[value],
                )
            });
            for stream in &streams {
                body.add_op(
                    block,
                    Operator::Call {
                        function_index: registry.web_streams.unwrap().dispose,
                    },
                    &[*stream],
                    &[],
                );
            }
            body.add_op(
                block,
                Operator::Call {
                    function_index: native.finish,
                },
                &[],
                &[],
            );
            if let Some(root) = root {
                root.release(&mut body, block);
            }
        }
    }
    if registry.promises.is_none() {
        for block in [outcome.ok_block, outcome.err_block] {
            for stream in &streams {
                body.add_op(
                    block,
                    Operator::Call {
                        function_index: registry.web_streams.unwrap().dispose,
                    },
                    &[*stream],
                    &[],
                );
            }
        }
    }
    if let Some(fetch) = registry.fetch_helpers {
        for block in [outcome.ok_block, outcome.err_block] {
            body.add_op(
                block,
                Operator::Call {
                    function_index: fetch.finish,
                },
                &[],
                &[],
            );
        }
    }
    match export.convention {
        ExportConvention::ResolvedWit => unreachable!("resolved WIT uses its schema adapter"),
        ExportConvention::Direct if super::structured::is_string_array(callee.success_type()) => {
            let block = outcome.ok_block;
            let descriptor = decode_payload(&mut body, block, outcome.payload, true);
            let value = body.add_op(
                block,
                Operator::Call {
                    function_index: registry.structured_helpers.unwrap().lower_strings,
                },
                &[descriptor],
                &[Type::I32],
            );
            body.set_terminator(
                block,
                Terminator::Return {
                    values: vec![value],
                },
            );
            body.set_terminator(outcome.err_block, Terminator::Unreachable);
        }
        ExportConvention::Direct
            if super::text_or_bytes::is_text_or_bytes(callee.success_type()) =>
        {
            let block = outcome.ok_block;
            let address = body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
            let value = decode_payload(&mut body, block, outcome.payload, true);
            store_text_or_bytes(&mut body, block, memory, address, value, 0);
            body.set_terminator(
                block,
                Terminator::Return {
                    values: vec![address],
                },
            );
            body.set_terminator(outcome.err_block, Terminator::Unreachable);
        }
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
            for (block, status, ty, payload) in [
                (
                    outcome.ok_block,
                    CompletionStatus::Returned,
                    success,
                    outcome.payload,
                ),
                (
                    outcome.err_block,
                    CompletionStatus::Threw,
                    ValuePayload::Number,
                    error_payload,
                ),
            ] {
                let retptr = emit_retptr_store(&mut body, block, registry, status, payload, ty);
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

/// Stores a WIT Result tag and its declared payload at the return pointer.
/// Numeric error payloads align the union to eight bytes, including boolean success variants.
fn emit_retptr_store(
    body: &mut FunctionBody,
    block: Block,
    registry: &ModuleRegistry,
    status: CompletionStatus,
    payload_f64: Value,
    payload_type: ValuePayload,
) -> Value {
    let memory = registry.memory;
    let addr = if payload_type == ValuePayload::Stats {
        let zero = body.add_op(block, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let alignment = body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
        let size = body.add_op(block, Operator::I32Const { value: 32 }, &[], &[Type::I32]);
        body.add_op(
            block,
            Operator::Call {
                function_index: registry.allocator.unwrap().realloc,
            },
            &[zero, zero, alignment, size],
            &[Type::I32],
        )
    } else {
        body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32])
    };
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
    match payload_type {
        ValuePayload::Stats => {
            let descriptor = decode_payload(body, block, payload_f64, true);
            let eight = body.add_op(block, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
            let destination = body.add_op(block, Operator::I32Add, &[addr, eight], &[Type::I32]);
            let size = body.add_op(block, Operator::I32Const { value: 24 }, &[], &[Type::I32]);
            body.add_op(
                block,
                Operator::MemoryCopy {
                    src_mem: memory,
                    dst_mem: memory,
                },
                &[destination, descriptor, size],
                &[],
            );
        }
        ValuePayload::StringArray => {
            let descriptor = decode_payload(body, block, payload_f64, true);
            let canonical = body.add_op(
                block,
                Operator::Call {
                    function_index: registry.structured_helpers.unwrap().lower_strings,
                },
                &[descriptor],
                &[Type::I32],
            );
            store_sequence(body, block, memory, addr, canonical, 8);
        }
        ValuePayload::Number => {
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
        }
        ValuePayload::Boolean => {
            let payload = decode_payload(body, block, payload_f64, true);
            body.add_op(
                block,
                Operator::I32Store8 {
                    memory: waffle::MemoryArg {
                        align: 0,
                        offset: 8,
                        memory,
                    },
                },
                &[addr, payload],
                &[],
            );
        }
        ValuePayload::String | ValuePayload::Bytes => {
            let descriptor = decode_payload(body, block, payload_f64, true);
            store_sequence(body, block, memory, addr, descriptor, 8);
        }
        ValuePayload::TextOrBytes => {
            let value = decode_payload(body, block, payload_f64, true);
            store_text_or_bytes(body, block, memory, addr, value, 8);
        }
    }
    addr
}

/// Result of emitting an internal call branch.
pub(crate) struct InternalCallOutcome {
    pub(crate) ok_block: Block,
    pub(crate) err_block: Block,
    pub(crate) payload: Value,
}

/// Emits an internal call with `(status: i32, payload: f64)` splitting into `ok_block` and `err_block`.
pub(crate) fn emit_fallible_call(
    body: &mut FunctionBody,
    current_block: Block,
    callee: Func,
    args: &[Value],
) -> InternalCallOutcome {
    let call_val = body.add_op(
        current_block,
        Operator::Call {
            function_index: callee,
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
    body.blocks[ok_block].desc = format!("call {callee} ok");
    let err_block = body.add_block();
    body.blocks[err_block].desc = format!("call {callee} err");

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

fn store_text_or_bytes(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    address: Value,
    value: Value,
    offset: u32,
) {
    let one = body.add_op(block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let mask = body.add_op(block, Operator::I32Const { value: !1 }, &[], &[Type::I32]);
    let tag = body.add_op(block, Operator::I32And, &[value, one], &[Type::I32]);
    let descriptor = body.add_op(block, Operator::I32And, &[value, mask], &[Type::I32]);
    body.add_op(
        block,
        Operator::I32Store8 {
            memory: waffle::MemoryArg {
                align: 0,
                offset,
                memory,
            },
        },
        &[address, tag],
        &[],
    );
    store_sequence(body, block, memory, address, descriptor, offset + 4);
}

fn store_sequence(
    body: &mut FunctionBody,
    block: Block,
    memory: waffle::Memory,
    address: Value,
    descriptor: Value,
    offset: u32,
) {
    for field in [0, 4] {
        let value = body.add_op(
            block,
            Operator::I32Load {
                memory: waffle::MemoryArg {
                    align: 2,
                    offset: field,
                    memory,
                },
            },
            &[descriptor],
            &[Type::I32],
        );
        body.add_op(
            block,
            Operator::I32Store {
                memory: waffle::MemoryArg {
                    align: 2,
                    offset: offset + field,
                    memory,
                },
            },
            &[address, value],
            &[],
        );
    }
}
