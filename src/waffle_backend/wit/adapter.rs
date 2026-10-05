//! Canonical values and typed guest values share the invocation allocator.

use super::{WitExport, WitWorld};
use crate::{
    sdk::codegen::to_camel_case,
    waffle_backend::{
        abi,
        allocation::RetainedValues,
        registry::{FunctionExport, FunctionInfo, ModuleRegistry},
        strings::StringPool,
        values::ValueTag,
    },
};
use anyhow::{Result, bail, ensure};
use waffle::{
    Block, BlockTarget, Func, FunctionBody, MemoryArg, Module, Operator, Terminator,
    Type as CoreType, Value,
};
use wit_parser::{Int, SizeAlign, Type, TypeDefKind};

mod flags;
mod lists;
mod variants;

pub(in crate::waffle_backend) fn build_export_wrapper(
    module: &Module<'static>,
    callee: &FunctionInfo,
    export: &FunctionExport,
    registry: &ModuleRegistry,
    wit: &WitWorld,
    declaration: &WitExport,
    strings: &StringPool,
) -> Result<FunctionBody> {
    let mut sizes = SizeAlign::default();
    sizes.fill(&wit.resolve)?;
    let mut body = FunctionBody::new(module, export.sig);
    let block = body.entry;
    let command_failure =
        (declaration.core_name == "wasi:cli/run@0.3.0#run").then(|| body.add_block());
    let cancellation_failure =
        if declaration.function.kind.is_async() && registry.callbacks.is_some() {
            Some(body.add_block())
        } else {
            None
        };
    let mut adapter = Adapter {
        cancellation_failure,
        body,
        block,
        registry,
        wit,
        sizes,
        scratch: None,
        command_failure,
        strings,
    };
    let signature = wit.resolve.wasm_signature(
        wit_parser::abi::AbiVariant::GuestExport,
        &declaration.function,
    );
    let params: Vec<_> = adapter.body.blocks[block]
        .params
        .iter()
        .map(|param| param.1)
        .collect();
    if let Some(native) = registry.promises.as_ref().map(|runtime| &runtime.native) {
        adapter.body.add_op(
            adapter.block,
            Operator::Call {
                function_index: native.enter,
            },
            &[],
            &[],
        );
    } else {
        let address = adapter.integer(32);
        let active = adapter.load_i32(address, 0);
        let inactive = adapter.op(Operator::I32Eqz, &[active], CoreType::I32);
        adapter.require(inactive);
        let one = adapter.integer(1);
        adapter.store_i32(address, 0, one);
    }
    let mut source = if signature.indirect_params {
        Input::Memory {
            pointer: params[0],
            offset: 0,
        }
    } else {
        Input::Flat {
            values: &params,
            cursor: 0,
        }
    };
    let offsets = adapter
        .sizes
        .field_offsets(declaration.function.params.iter().map(|param| &param.ty));
    let mut args = Vec::new();
    for (param, (offset, _)) in declaration.function.params.iter().zip(offsets) {
        if let Input::Memory {
            offset: position, ..
        } = &mut source
        {
            *position = offset.size_wasm32() as u32;
        }
        args.push(adapter.lift(param.ty, &mut source)?);
    }
    if let Some(state) = &registry.module_state {
        let allocator = registry.allocator.unwrap();
        let mut references = Vec::new();
        for (argument, param) in args.iter().zip(&declaration.function.params) {
            if crate::waffle_backend::ssa::types::is_reference(&super::hir_type(
                &wit.resolve,
                param.ty,
            )?) {
                references.push(*argument);
            }
        }
        let roots = (!references.is_empty()).then(|| {
            RetainedValues::new(
                &mut adapter.body,
                adapter.block,
                registry.memory,
                allocator,
                &references,
            )
        });
        adapter.call_checked(state.evaluate, &[]);
        if let Some(roots) = roots {
            roots.release(&mut adapter.body, adapter.block);
        }
    }
    let payload = adapter.call_checked(callee.func_index, &args);
    let result = declaration
        .function
        .result
        .map(|ty| adapter.decode(ty, payload))
        .transpose()?;
    let result_root = if let Some(ty) = declaration.function.result
        && crate::waffle_backend::ssa::types::is_reference(&super::hir_type(&wit.resolve, ty)?)
    {
        Some(RetainedValues::new(
            &mut adapter.body,
            adapter.block,
            registry.memory,
            registry.allocator.unwrap(),
            &[result.unwrap()],
        ))
    } else {
        None
    };
    adapter.finish_invocation();
    if declaration.core_name == "wasi:cli/run@0.3.0#run"
        && let Some(function) = registry.finish_command
    {
        adapter.body.add_op(
            adapter.block,
            Operator::Call {
                function_index: function,
            },
            &[],
            &[],
        );
    }
    let mut returned = Vec::new();
    if let Some(ty) = declaration.function.result {
        let value = result.unwrap();
        let size = adapter.sizes.size(&ty).size_wasm32() as u32;
        let alignment = adapter.sizes.align(&ty).align_wasm32() as u32;
        let address = adapter.allocate(size.max(1), alignment);
        adapter.lower(ty, value, address, 0)?;
        if signature.retptr {
            returned.push(address);
        } else {
            adapter.flatten_memory(ty, address, 0, &mut returned)?;
        }
    }
    if let Some(root) = result_root {
        root.release(&mut adapter.body, adapter.block);
    }
    adapter
        .body
        .set_terminator(adapter.block, Terminator::Return { values: returned });
    if let Some(failure) = command_failure {
        adapter.block = failure;
        adapter.finish_invocation();
        let failed = adapter.integer(1);
        adapter.body.set_terminator(
            adapter.block,
            Terminator::Return {
                values: vec![failed],
            },
        );
    }
    if let Some(failure) = cancellation_failure {
        adapter.block = failure;
        let cancelled = adapter.call(registry.operations.unwrap().cancelled, &[]);
        adapter.require(cancelled);
        let mut values = Vec::new();
        for ty in &module.signatures[export.sig].returns {
            let op = match ty {
                CoreType::I32 => Operator::I32Const { value: 0 },
                CoreType::I64 => Operator::I64Const { value: 0 },
                CoreType::F32 => Operator::F32Const { value: 0 },
                CoreType::F64 => Operator::F64Const { value: 0 },
                _ => unreachable!(),
            };
            values.push(adapter.op(op, &[], *ty));
        }
        adapter
            .body
            .set_terminator(adapter.block, Terminator::Return { values });
    }
    adapter.body.validate()?;
    adapter.body.verify_reducible()?;
    Ok(adapter.body)
}

enum Input<'a> {
    Flat { values: &'a [Value], cursor: usize },
    Memory { pointer: Value, offset: u32 },
}

struct Adapter<'a> {
    body: FunctionBody,
    block: Block,
    registry: &'a ModuleRegistry,
    wit: &'a WitWorld,
    sizes: SizeAlign,
    scratch: Option<crate::waffle_backend::allocation::scope::ScratchScope>,
    strings: &'a StringPool,
    command_failure: Option<Block>,
    cancellation_failure: Option<Block>,
}

impl Adapter<'_> {
    fn finish_invocation(&mut self) {
        let after = self.cancellation_failure.map(|_| self.body.add_block());
        if let Some(after) = after {
            let cancelled = self.call(self.registry.operations.unwrap().cancelled, &[]);
            let finish = self.body.add_block();
            self.body.set_terminator(
                self.block,
                Terminator::CondBr {
                    cond: cancelled,
                    if_true: BlockTarget {
                        block: after,
                        args: vec![],
                    },
                    if_false: BlockTarget {
                        block: finish,
                        args: vec![],
                    },
                },
            );
            self.block = finish;
        }
        if let Some(native) = self
            .registry
            .promises
            .as_ref()
            .map(|runtime| &runtime.native)
        {
            self.body.add_op(
                self.block,
                Operator::Call {
                    function_index: if self.cancellation_failure.is_some() {
                        native.validate
                    } else {
                        native.finish
                    },
                },
                &[],
                &[],
            );
        } else {
            let address = self.integer(32);
            let zero = self.integer(0);
            self.store_i32(address, 0, zero);
        }
        if let Some(fetch) = self.registry.http_helpers.and_then(|http| http.fetch) {
            self.body.add_op(
                self.block,
                Operator::Call {
                    function_index: fetch.finish,
                },
                &[],
                &[],
            );
        }
        if let Some(after) = after {
            self.body.set_terminator(
                self.block,
                Terminator::Br {
                    target: BlockTarget {
                        block: after,
                        args: vec![],
                    },
                },
            );
            self.block = after;
        }
    }

    fn op(&mut self, operator: Operator, args: &[Value], ty: CoreType) -> Value {
        self.body.add_op(self.block, operator, args, &[ty])
    }
    fn integer(&mut self, value: u32) -> Value {
        self.op(Operator::I32Const { value }, &[], CoreType::I32)
    }
    fn number(&mut self, value: f64) -> Value {
        self.op(
            Operator::F64Const {
                value: value.to_bits(),
            },
            &[],
            CoreType::F64,
        )
    }
    fn call(&mut self, function_index: Func, args: &[Value]) -> Value {
        let value = self.op(Operator::Call { function_index }, args, CoreType::I32);
        if self
            .registry
            .allocator
            .is_some_and(|allocator| allocator.realloc == function_index)
            && let Some(scratch) = &self.scratch
        {
            scratch.retain(&mut self.body, self.block, value);
        }
        value
    }
    fn call_checked(&mut self, function: Func, args: &[Value]) -> Value {
        let outcome = abi::emit_fallible_call(&mut self.body, self.block, function, args);
        let failure = if let Some(block) = self.cancellation_failure {
            let ordinary = self.command_failure.unwrap_or_else(|| {
                let trap = self.body.add_block();
                self.body.set_terminator(trap, Terminator::Unreachable);
                trap
            });
            let saved = self.block;
            self.block = outcome.err_block;
            let cancelled = self.call(self.registry.operations.unwrap().cancelled, &[]);
            self.block = saved;
            Terminator::CondBr {
                cond: cancelled,
                if_true: BlockTarget {
                    block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: ordinary,
                    args: vec![],
                },
            }
        } else if let Some(block) = self.command_failure {
            Terminator::Br {
                target: BlockTarget {
                    block,
                    args: vec![],
                },
            }
        } else {
            Terminator::Unreachable
        };
        self.body.set_terminator(outcome.err_block, failure);
        self.block = outcome.ok_block;
        outcome.payload
    }
    fn require(&mut self, condition: Value) {
        let next = self.body.add_block();
        let trap = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: condition,
                if_true: BlockTarget {
                    block: next,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: trap,
                    args: vec![],
                },
            },
        );
        self.body.set_terminator(trap, Terminator::Unreachable);
        self.block = next;
    }
    fn memory(&self, offset: u32) -> MemoryArg {
        MemoryArg {
            memory: self.registry.memory,
            offset,
            align: 0,
        }
    }
    fn load_i32(&mut self, pointer: Value, offset: u32) -> Value {
        self.op(
            Operator::I32Load {
                memory: self.memory(offset),
            },
            &[pointer],
            CoreType::I32,
        )
    }
    fn store_i32(&mut self, pointer: Value, offset: u32, value: Value) {
        self.body.add_op(
            self.block,
            Operator::I32Store {
                memory: self.memory(offset),
            },
            &[pointer, value],
            &[],
        );
    }
    fn allocate(&mut self, size: u32, alignment: u32) -> Value {
        let zero = self.integer(0);
        let alignment = self.integer(alignment);
        let size = self.integer(size);
        self.call(
            self.registry.allocator.unwrap().realloc,
            &[zero, zero, alignment, size],
        )
    }
    fn tag(&self, ty: Type) -> Result<ValueTag> {
        ValueTag::of(&super::hir_type(&self.wit.resolve, ty)?)
    }
    fn alias(&self, mut ty: Type) -> Type {
        while let Type::Id(id) = ty {
            if let TypeDefKind::Type(inner) = self.wit.resolve.types[id].kind {
                ty = inner;
            } else {
                break;
            }
        }
        ty
    }
    fn read_scalar(&mut self, ty: Type, source: &mut Input<'_>) -> Result<Value> {
        match source {
            Input::Flat { values, cursor } => {
                let value = values[*cursor];
                *cursor += 1;
                let target = match self.alias(ty) {
                    Type::F32 => CoreType::F32,
                    Type::F64 => CoreType::F64,
                    Type::U64 | Type::S64 => CoreType::I64,
                    _ => CoreType::I32,
                };
                self.convert_flat(value, target)
            }
            Input::Memory { pointer, offset } => self.load_scalar(ty, *pointer, *offset),
        }
    }
    fn load_scalar(&mut self, ty: Type, pointer: Value, offset: u32) -> Result<Value> {
        let memory = self.memory(offset);
        let (op, result) = match self.alias(ty) {
            Type::Bool | Type::U8 => (Operator::I32Load8U { memory }, CoreType::I32),
            Type::S8 => (Operator::I32Load8S { memory }, CoreType::I32),
            Type::U16 => (Operator::I32Load16U { memory }, CoreType::I32),
            Type::S16 => (Operator::I32Load16S { memory }, CoreType::I32),
            Type::U32 | Type::S32 => (Operator::I32Load { memory }, CoreType::I32),
            Type::F32 => (Operator::F32Load { memory }, CoreType::F32),
            Type::F64 => (Operator::F64Load { memory }, CoreType::F64),
            Type::U64 | Type::S64 => (Operator::I64Load { memory }, CoreType::I64),
            Type::Id(id) if matches!(self.wit.resolve.types[id].kind, TypeDefKind::Enum(_)) => {
                match self.sizes.size(&Type::Id(id)).size_wasm32() {
                    1 => (Operator::I32Load8U { memory }, CoreType::I32),
                    2 => (Operator::I32Load16U { memory }, CoreType::I32),
                    _ => (Operator::I32Load { memory }, CoreType::I32),
                }
            }
            other => bail!("WIT value is not a scalar: {other:?}"),
        };
        Ok(self.op(op, &[pointer], result))
    }
    fn lift(&mut self, ty: Type, source: &mut Input<'_>) -> Result<Value> {
        let ty = self.alias(ty);
        if matches!(ty, Type::U64 | Type::S64) {
            let value = self.read_scalar(ty, source)?;
            let pointer = self.allocate(8, 8);
            self.body.add_op(
                self.block,
                Operator::I64Store {
                    memory: self.memory(0),
                },
                &[pointer, value],
                &[],
            );
            return Ok(pointer);
        }
        if let Some(shape) = self.variant(ty) {
            return self.lift_variant(ty, shape, source);
        }
        if let Type::Id(id) = ty {
            return match self.wit.resolve.types[id].kind.clone() {
                TypeDefKind::Flags(flags) => self.lift_flags(&flags, source),
                TypeDefKind::Record(record) => {
                    let object = self.call(self.registry.object_helpers.unwrap().new, &[]);
                    let offsets = self
                        .sizes
                        .field_offsets(record.fields.iter().map(|field| &field.ty));
                    let base = match source {
                        Input::Memory { offset, .. } => *offset,
                        _ => 0,
                    };
                    for (field, (offset, _)) in record.fields.iter().zip(offsets) {
                        if let Input::Memory {
                            offset: position, ..
                        } = source
                        {
                            *position = base + offset.size_wasm32() as u32;
                        }
                        let value = self.lift(field.ty, source)?;
                        self.set_field(object, &to_camel_case(&field.name), field.ty, value)?;
                    }
                    Ok(object)
                }
                TypeDefKind::List(inner) => self.lift_list(inner, source),
                TypeDefKind::Tuple(tuple) => {
                    let length = self.integer(tuple.types.len() as u32);
                    let array = self.call(self.registry.value_access.unwrap().array_new, &[length]);
                    let base = match source {
                        Input::Memory { offset, .. } => *offset,
                        _ => 0,
                    };
                    let offsets = self.sizes.field_offsets(&tuple.types);
                    for (index, (offset, ty)) in offsets.into_iter().enumerate() {
                        if let Input::Memory {
                            offset: position, ..
                        } = source
                        {
                            *position = base + offset.size_wasm32() as u32;
                        }
                        let value = self.lift(*ty, source)?;
                        let boxed = self.box_value(*ty, value)?;
                        self.store_i32(array, 8 + index as u32 * 4, boxed);
                    }
                    Ok(array)
                }
                TypeDefKind::Enum(enumeration) => {
                    let index = self.read_scalar(ty, source)?;
                    let join = self.body.add_block();
                    let result = self.body.add_blockparam(join, CoreType::I32);
                    for (position, case) in enumeration.cases.iter().enumerate() {
                        let position = self.integer(position as u32);
                        let equal = self.op(Operator::I32Eq, &[index, position], CoreType::I32);
                        let selected = self.body.add_block();
                        let next = self.body.add_block();
                        self.body.set_terminator(
                            self.block,
                            Terminator::CondBr {
                                cond: equal,
                                if_true: BlockTarget {
                                    block: selected,
                                    args: vec![],
                                },
                                if_false: BlockTarget {
                                    block: next,
                                    args: vec![],
                                },
                            },
                        );
                        self.block = selected;
                        let value = self.integer(self.strings.get(&case.name).unwrap());
                        self.body.set_terminator(
                            selected,
                            Terminator::Br {
                                target: BlockTarget {
                                    block: join,
                                    args: vec![value],
                                },
                            },
                        );
                        self.block = next;
                    }
                    self.body
                        .set_terminator(self.block, Terminator::Unreachable);
                    self.block = join;
                    Ok(result)
                }
                other => bail!("Unsupported resolved WIT parameter: {other:?}"),
            };
        }
        if ty == Type::String {
            let (pointer, length) = match source {
                Input::Flat { values, cursor } => {
                    let pair = (
                        self.convert_flat(values[*cursor], CoreType::I32)?,
                        self.convert_flat(values[*cursor + 1], CoreType::I32)?,
                    );
                    *cursor += 2;
                    pair
                }
                Input::Memory { pointer, offset } => (
                    self.load_i32(*pointer, *offset),
                    self.load_i32(*pointer, *offset + 4),
                ),
            };
            return Ok(self.call(
                self.registry.string_helpers.unwrap().lift_canonical,
                &[pointer, length],
            ));
        }
        let value = self.read_scalar(ty, source)?;
        Ok(match ty {
            Type::Bool | Type::F64 => value,
            Type::F32 => self.op(Operator::F64PromoteF32, &[value], CoreType::F64),
            Type::S8 | Type::S16 | Type::S32 => {
                self.op(Operator::F64ConvertI32S, &[value], CoreType::F64)
            }
            _ => self.op(Operator::F64ConvertI32U, &[value], CoreType::F64),
        })
    }
    fn field(&mut self, object: Value, name: &str, tag: ValueTag) -> Value {
        let key = self.integer(self.strings.get(name).unwrap());
        let entry = self.call(self.registry.object_helpers.unwrap().get, &[object, key]);
        let tag_number = self.integer(tag as u32);
        let optional = self.integer(0);
        let payload = self.call_checked(
            self.registry.object_helpers.unwrap().value,
            &[entry, tag_number, optional],
        );
        abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            tag as u32 != ValueTag::Number as u32,
        )
    }
    fn lower(&mut self, ty: Type, value: Value, pointer: Value, offset: u32) -> Result<()> {
        let ty = self.alias(ty);
        let memory = self.memory(offset);
        if matches!(ty, Type::U64 | Type::S64) {
            let value = self.load_scalar(ty, value, 0)?;
            self.body.add_op(
                self.block,
                Operator::I64Store { memory },
                &[pointer, value],
                &[],
            );
            return Ok(());
        }
        if let Some(shape) = self.variant(ty) {
            return self.lower_variant(shape, value, pointer, offset);
        }
        if let Type::Id(id) = ty {
            match self.wit.resolve.types[id].kind.clone() {
                TypeDefKind::Flags(flags) => self.lower_flags(&flags, value, pointer, offset)?,
                TypeDefKind::List(inner) => self.lower_list(inner, value, pointer, offset)?,
                TypeDefKind::Record(record) => {
                    let offsets = self
                        .sizes
                        .field_offsets(record.fields.iter().map(|field| &field.ty));
                    for (field, (position, _)) in record.fields.iter().zip(offsets) {
                        let child =
                            self.read_field(value, &to_camel_case(&field.name), field.ty)?;
                        self.lower(
                            field.ty,
                            child,
                            pointer,
                            offset + position.size_wasm32() as u32,
                        )?;
                    }
                }
                TypeDefKind::Tuple(tuple) => {
                    let length = self.load_i32(value, 4);
                    let expected = self.integer(tuple.types.len() as u32);
                    let equal = self.op(Operator::I32Eq, &[length, expected], CoreType::I32);
                    self.require(equal);
                    let data = self.load_i32(value, 0);
                    for (index, (position, ty)) in self
                        .sizes
                        .field_offsets(&tuple.types)
                        .into_iter()
                        .enumerate()
                    {
                        let boxed = self.load_i32(data, index as u32 * 4);
                        let child = self.extract(*ty, boxed)?;
                        self.lower(*ty, child, pointer, offset + position.size_wasm32() as u32)?;
                    }
                }
                TypeDefKind::Enum(enumeration) => {
                    let join = self.body.add_block();
                    for (position, case) in enumeration.cases.iter().enumerate() {
                        let case_value = self.integer(self.strings.get(&case.name).unwrap());
                        let comparison = self.call(
                            self.registry.string_helpers.unwrap().str_compare,
                            &[value, case_value],
                        );
                        let equal = self.op(Operator::I32Eqz, &[comparison], CoreType::I32);
                        let selected = self.body.add_block();
                        let next = self.body.add_block();
                        self.body.set_terminator(
                            self.block,
                            Terminator::CondBr {
                                cond: equal,
                                if_true: BlockTarget {
                                    block: selected,
                                    args: vec![],
                                },
                                if_false: BlockTarget {
                                    block: next,
                                    args: vec![],
                                },
                            },
                        );
                        self.block = selected;
                        let discriminant = self.integer(position as u32);
                        let operator = match self.sizes.size(&ty).size_wasm32() {
                            1 => Operator::I32Store8 { memory },
                            2 => Operator::I32Store16 { memory },
                            _ => Operator::I32Store { memory },
                        };
                        self.body
                            .add_op(self.block, operator, &[pointer, discriminant], &[]);
                        self.body.set_terminator(
                            self.block,
                            Terminator::Br {
                                target: BlockTarget {
                                    block: join,
                                    args: vec![],
                                },
                            },
                        );
                        self.block = next;
                    }
                    self.body
                        .set_terminator(self.block, Terminator::Unreachable);
                    self.block = join;
                }
                other => bail!("Unsupported resolved WIT result: {other:?}"),
            }
            return Ok(());
        }
        if ty == Type::String {
            let data = self.load_i32(value, 0);
            let length = self.load_i32(value, 4);
            self.store_i32(pointer, offset, data);
            self.store_i32(pointer, offset + 4, length);
            return Ok(());
        }
        let (operator, value) = match ty {
            Type::Bool => (Operator::I32Store8 { memory }, value),
            Type::F64 => (Operator::F64Store { memory }, value),
            Type::F32 => (
                Operator::F32Store { memory },
                self.op(Operator::F32DemoteF64, &[value], CoreType::F32),
            ),
            _ => {
                let (minimum, maximum, signed) = match ty {
                    Type::U8 => (0., 255., false),
                    Type::S8 => (-128., 127., true),
                    Type::U16 => (0., 65535., false),
                    Type::S16 => (-32768., 32767., true),
                    Type::U32 => (0., 4294967295., false),
                    Type::S32 => (-2147483648., 2147483647., true),
                    _ => bail!("Unsupported WIT numeric value: {ty:?}"),
                };
                let min = self.number(minimum);
                let max = self.number(maximum);
                let low = self.op(Operator::F64Ge, &[value, min], CoreType::I32);
                let high = self.op(Operator::F64Le, &[value, max], CoreType::I32);
                let valid = self.op(Operator::I32And, &[low, high], CoreType::I32);
                self.require(valid);
                let truncated = self.op(Operator::F64Trunc, &[value], CoreType::F64);
                let whole = self.op(Operator::F64Eq, &[value, truncated], CoreType::I32);
                self.require(whole);
                let integer = self.op(
                    if signed {
                        Operator::I32TruncF64S
                    } else {
                        Operator::I32TruncF64U
                    },
                    &[value],
                    CoreType::I32,
                );
                (
                    match ty {
                        Type::U8 | Type::S8 => Operator::I32Store8 { memory },
                        Type::U16 | Type::S16 => Operator::I32Store16 { memory },
                        _ => Operator::I32Store { memory },
                    },
                    integer,
                )
            }
        };
        self.body
            .add_op(self.block, operator, &[pointer, value], &[]);
        Ok(())
    }
    fn flatten_memory(
        &mut self,
        ty: Type,
        pointer: Value,
        offset: u32,
        values: &mut Vec<Value>,
    ) -> Result<()> {
        let ty = self.alias(ty);
        if let Type::Id(id) = ty
            && let TypeDefKind::Flags(flags) = &self.wit.resolve.types[id].kind
        {
            values.push(self.load_scalar(flags::storage_type(flags), pointer, offset)?);
            return Ok(());
        }
        if matches!(ty,Type::Id(id) if matches!(self.wit.resolve.types[id].kind,TypeDefKind::List(_)))
        {
            values.push(self.load_i32(pointer, offset));
            values.push(self.load_i32(pointer, offset + 4));
            return Ok(());
        }
        if let Some(shape) = self.variant(ty) {
            return self.flatten_variant(ty, shape, pointer, offset, values);
        }
        if let Type::Id(id) = ty {
            let fields = match &self.wit.resolve.types[id].kind {
                TypeDefKind::Tuple(tuple) => Some(tuple.types.clone()),
                TypeDefKind::Record(record) => {
                    Some(record.fields.iter().map(|field| field.ty).collect())
                }
                TypeDefKind::Enum(_) => None,
                _ => bail!("WIT aggregate requires an indirect result"),
            };
            if let Some(fields) = fields {
                for (position, ty) in self.sizes.field_offsets(&fields) {
                    self.flatten_memory(
                        *ty,
                        pointer,
                        offset + position.size_wasm32() as u32,
                        values,
                    )?;
                }
                return Ok(());
            }
        }
        if ty == Type::String {
            values.push(self.load_i32(pointer, offset));
            values.push(self.load_i32(pointer, offset + 4));
            return Ok(());
        }
        values.push(self.load_scalar(ty, pointer, offset)?);
        Ok(())
    }
}

pub(in crate::waffle_backend) fn build_import_wrapper(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    wit: &WitWorld,
    import: &super::WitImport,
    callee: Func,
    strings: &StringPool,
) -> Result<Func> {
    let signature = wit.resolve.wasm_signature(import.abi(), &import.function);
    let params = import
        .function
        .params
        .iter()
        .map(|param| {
            crate::waffle_backend::registry::map_type_to_waffle(&super::hir_type(
                &wit.resolve,
                param.ty,
            )?)
        })
        .collect::<Result<Vec<_>>>()?;
    let sig = module.signatures.push(waffle::SignatureData {
        params,
        returns: vec![CoreType::I32, CoreType::F64],
    });
    let mut sizes = SizeAlign::default();
    sizes.fill(&wit.resolve)?;
    let mut body = FunctionBody::new(module, sig);
    let block = body.entry;
    let scratch = crate::waffle_backend::allocation::scope::ScratchScope::new(
        &mut body,
        block,
        registry.memory,
        registry.allocator.unwrap(),
    );
    let mut adapter = Adapter {
        body,
        block,
        registry,
        wit,
        sizes,
        scratch: Some(scratch),
        command_failure: None,
        cancellation_failure: None,
        strings,
    };
    let guest_params: Vec<_> = adapter.body.blocks[block]
        .params
        .iter()
        .map(|param| param.1)
        .collect();
    let layout = adapter
        .sizes
        .params(import.function.params.iter().map(|param| &param.ty));
    let pointer = adapter.allocate(
        (layout.size.size_wasm32() as u32).max(1),
        layout.align.align_wasm32() as u32,
    );
    let offsets = adapter
        .sizes
        .field_offsets(import.function.params.iter().map(|param| &param.ty));
    let mut args = Vec::new();
    for ((param, value), (offset, _)) in
        import.function.params.iter().zip(guest_params).zip(offsets)
    {
        let offset = offset.size_wasm32() as u32;
        adapter.lower(param.ty, value, pointer, offset)?;
        if !signature.indirect_params {
            adapter.flatten_memory(param.ty, pointer, offset, &mut args)?;
        }
    }
    if signature.indirect_params {
        args.push(pointer);
    }
    let return_pointer = if signature.retptr {
        let ty = import.function.result.unwrap();
        let pointer = adapter.allocate(
            (adapter.sizes.size(&ty).size_wasm32() as u32).max(1),
            adapter.sizes.align(&ty).align_wasm32() as u32,
        );
        args.push(pointer);
        Some(pointer)
    } else {
        None
    };
    let results: Vec<_> = signature
        .results
        .into_iter()
        .map(super::core_type)
        .collect();
    let returned = adapter.body.add_op(
        adapter.block,
        Operator::Call {
            function_index: callee,
        },
        &args,
        &results,
    );
    if import.function.kind.is_async() {
        let status = adapter.call(registry.await_subtask.unwrap(), &[returned]);
        let two = adapter.integer(2);
        let completed = adapter.op(Operator::I32Eq, &[status, two], CoreType::I32);
        let success = adapter.body.add_block();
        let cancelled = adapter.body.add_block();
        adapter.body.set_terminator(
            adapter.block,
            Terminator::CondBr {
                cond: completed,
                if_true: BlockTarget {
                    block: success,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: cancelled,
                    args: vec![],
                },
            },
        );
        let cancelled = adapter
            .scratch
            .as_ref()
            .unwrap()
            .release(&mut adapter.body, cancelled);
        adapter.block = cancelled;
        let reason = adapter.number(20.0);
        abi::emit_completion(
            &mut adapter.body,
            cancelled,
            abi::CompletionStatus::Threw,
            reason,
        );
        adapter.block = success;
    }
    let value = if let Some(ty) = import.function.result {
        let direct = [returned];
        let mut input = if let Some(pointer) = return_pointer {
            Input::Memory { pointer, offset: 0 }
        } else {
            Input::Flat {
                values: &direct,
                cursor: 0,
            }
        };
        Some(adapter.lift(ty, &mut input)?)
    } else {
        None
    };
    let payload = abi::encode_payload(&mut adapter.body, adapter.block, value);
    adapter.block = adapter
        .scratch
        .take()
        .unwrap()
        .release(&mut adapter.body, adapter.block);
    abi::emit_completion(
        &mut adapter.body,
        adapter.block,
        abi::CompletionStatus::Returned,
        payload,
    );
    adapter.body.validate()?;
    adapter.body.verify_reducible()?;
    Ok(module.funcs.push(waffle::FuncDecl::Body(
        sig,
        format!("{}.import", import.function.name),
        adapter.body,
    )))
}

/// Adapts a canonical synchronous result layout to the official task.return signature.
pub(in crate::waffle_backend) fn build_task_return(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    wit: &WitWorld,
    declaration: &WitExport,
    export: &FunctionExport,
    task_return: Func,
    strings: &StringPool,
) -> Result<Func> {
    use crate::waffle_backend::runtime::builder;
    let params = module.signatures[export.sig].returns.clone();
    let function = builder::declare(
        module,
        &format!("{}.task-return", export.name),
        &params,
        &[],
    );
    let mut sizes = SizeAlign::default();
    sizes.fill(&wit.resolve)?;
    let body = FunctionBody::new(module, module.funcs[function].sig());
    let block = body.entry;
    let mut adapter = Adapter {
        body,
        block,
        registry,
        wit,
        sizes,
        scratch: None,
        strings,
        command_failure: None,
        cancellation_failure: None,
    };
    let params = adapter.body.blocks[block]
        .params
        .iter()
        .map(|param| param.1)
        .collect::<Vec<_>>();
    let signature = wit.resolve.wasm_signature(
        wit_parser::abi::AbiVariant::GuestExport,
        &declaration.function,
    );
    let (_, _, returned) =
        declaration
            .function
            .task_return_import(&wit.resolve, None, wit_parser::Mangling::Legacy);
    let mut args = Vec::new();
    if signature.retptr && !returned.indirect_params {
        adapter.flatten_memory(
            declaration.function.result.unwrap(),
            params[0],
            0,
            &mut args,
        )?;
    } else {
        args = params;
    }
    adapter.body.add_op(
        adapter.block,
        Operator::Call {
            function_index: task_return,
        },
        &args,
        &[],
    );
    adapter
        .body
        .set_terminator(adapter.block, Terminator::Return { values: vec![] });
    adapter.body.validate()?;
    adapter.body.verify_reducible()?;
    let sig = module.funcs[function].sig();
    module.funcs[function] =
        waffle::FuncDecl::Body(sig, format!("{}.task-return", export.name), adapter.body);
    Ok(function)
}
