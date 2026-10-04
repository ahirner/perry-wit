//! Canonical values and typed guest values share the invocation allocator.

use super::{WitExport, WitExports};
use crate::{
    sdk::codegen::to_camel_case,
    waffle_backend::{
        abi,
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

pub(in crate::waffle_backend) fn build_export_wrapper(
    module: &Module<'static>,
    callee: &FunctionInfo,
    export: &FunctionExport,
    registry: &ModuleRegistry,
    wit: &WitExports,
    declaration: &WitExport,
    strings: &StringPool,
) -> Result<FunctionBody> {
    let mut sizes = SizeAlign::default();
    sizes.fill(&wit.resolve)?;
    let body = FunctionBody::new(module, export.sig);
    let block = body.entry;
    let mut adapter = Adapter {
        body,
        block,
        registry,
        wit,
        sizes,
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
    let payload = adapter.call_checked(callee.func_index, &args);
    let mut returned = Vec::new();
    if let Some(ty) = declaration.function.result {
        let reference = adapter.tag(ty)? as u32 != ValueTag::Number as u32;
        let value = abi::decode_payload(&mut adapter.body, adapter.block, payload, reference);
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
    adapter
        .body
        .set_terminator(adapter.block, Terminator::Return { values: returned });
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
    wit: &'a WitExports,
    sizes: SizeAlign,
    strings: &'a StringPool,
}

impl Adapter<'_> {
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
        self.op(Operator::Call { function_index }, args, CoreType::I32)
    }
    fn call_checked(&mut self, function: Func, args: &[Value]) -> Value {
        let outcome = abi::emit_fallible_call(&mut self.body, self.block, function, args);
        self.body
            .set_terminator(outcome.err_block, Terminator::Unreachable);
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
                Ok(value)
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
        if let Type::Id(id) = ty {
            return match self.wit.resolve.types[id].kind.clone() {
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
                        let key =
                            self.integer(self.strings.get(&to_camel_case(&field.name)).unwrap());
                        let tag = self.integer(self.tag(field.ty)? as u32);
                        let payload = abi::encode_payload(&mut self.body, self.block, Some(value));
                        self.call_checked(
                            self.registry.object_helpers.unwrap().set,
                            &[object, key, tag, payload],
                        );
                    }
                    Ok(object)
                }
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
                        let tag = self.integer(self.tag(*ty)? as u32);
                        let payload = abi::encode_payload(&mut self.body, self.block, Some(value));
                        let boxed =
                            self.call(self.registry.value_helpers.unwrap().new, &[tag, payload]);
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
                    let pair = (values[*cursor], values[*cursor + 1]);
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
        if let Type::Id(id) = ty {
            match self.wit.resolve.types[id].kind.clone() {
                TypeDefKind::Record(record) => {
                    let offsets = self
                        .sizes
                        .field_offsets(record.fields.iter().map(|field| &field.ty));
                    for (field, (position, _)) in record.fields.iter().zip(offsets) {
                        let child =
                            self.field(value, &to_camel_case(&field.name), self.tag(field.ty)?);
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
                        let tag = self.integer(self.tag(*ty)? as u32);
                        let payload = self.call_checked(
                            self.registry.value_helpers.unwrap().extract,
                            &[boxed, tag],
                        );
                        let reference = self.tag(*ty)? as u32 != ValueTag::Number as u32;
                        let child =
                            abi::decode_payload(&mut self.body, self.block, payload, reference);
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
                TypeDefKind::Result(result) => {
                    let ok = self.field(value, "ok", ValueTag::Boolean);
                    let yes = self.body.add_block();
                    let no = self.body.add_block();
                    let join = self.body.add_block();
                    self.body.set_terminator(
                        self.block,
                        Terminator::CondBr {
                            cond: ok,
                            if_true: BlockTarget {
                                block: yes,
                                args: vec![],
                            },
                            if_false: BlockTarget {
                                block: no,
                                args: vec![],
                            },
                        },
                    );
                    let payload_offset = self
                        .sizes
                        .payload_offset(Int::U8, [result.ok.as_ref(), result.err.as_ref()])
                        .size_wasm32() as u32;
                    for (block, discriminant, name, ty) in
                        [(yes, 0, "value", result.ok), (no, 1, "error", result.err)]
                    {
                        self.block = block;
                        let tag = self.integer(discriminant);
                        self.body.add_op(
                            block,
                            Operator::I32Store8 { memory },
                            &[pointer, tag],
                            &[],
                        );
                        if let Some(ty) = ty {
                            let child = self.field(value, name, self.tag(ty)?);
                            self.lower(ty, child, pointer, offset + payload_offset)?;
                        }
                        self.body.set_terminator(
                            self.block,
                            Terminator::Br {
                                target: BlockTarget {
                                    block: join,
                                    args: vec![],
                                },
                            },
                        );
                    }
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
        if let Type::Id(id) = ty {
            let fields = match &self.wit.resolve.types[id].kind {
                TypeDefKind::Tuple(tuple) => Some(tuple.types.clone()),
                TypeDefKind::Record(record) => {
                    Some(record.fields.iter().map(|field| field.ty).collect())
                }
                TypeDefKind::Enum(_) => None,
                TypeDefKind::Result(_) if self.sizes.size(&ty).size_wasm32() == 1 => {
                    values.push(self.load_scalar(Type::U8, pointer, offset)?);
                    return Ok(());
                }
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
        ensure!(ty != Type::String, "WIT strings require an indirect result");
        values.push(self.load_scalar(ty, pointer, offset)?);
        Ok(())
    }
}
