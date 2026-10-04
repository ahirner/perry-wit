//! Canonical variants share checked discriminants, payload layouts, and flat joins.

use super::*;
use wit_parser::abi::{FlatTypes, WasmType};

#[derive(Clone, Copy)]
enum VariantKind {
    Option,
    Result,
    Named,
}

pub(super) struct VariantShape {
    kind: VariantKind,
    tag: Int,
    cases: Vec<(String, Option<Type>)>,
    payload_offset: u32,
}

impl Adapter<'_> {
    pub(super) fn variant(&self, ty: Type) -> Option<VariantShape> {
        let Type::Id(id) = self.alias(ty) else {
            return None;
        };
        let (kind, tag, cases) = match &self.wit.resolve.types[id].kind {
            TypeDefKind::Option(inner) => (
                VariantKind::Option,
                Int::U8,
                vec![("none".into(), None), ("some".into(), Some(*inner))],
            ),
            TypeDefKind::Result(result) => (
                VariantKind::Result,
                Int::U8,
                vec![("value".into(), result.ok), ("error".into(), result.err)],
            ),
            TypeDefKind::Variant(variant) => (
                VariantKind::Named,
                variant.tag(),
                variant
                    .cases
                    .iter()
                    .map(|case| (case.name.clone(), case.ty))
                    .collect(),
            ),
            _ => return None,
        };
        let payload_offset = self
            .sizes
            .payload_offset(tag, cases.iter().map(|(_, ty)| ty.as_ref()))
            .size_wasm32() as u32;
        Some(VariantShape {
            kind,
            tag,
            cases,
            payload_offset,
        })
    }

    pub(super) fn is_nullable(&self, ty: Type) -> bool {
        matches!(self.alias(ty), Type::Id(id) if matches!(self.wit.resolve.types[id].kind, TypeDefKind::Option(_)))
    }

    pub(super) fn tagged(&mut self, ty: Type, value: Value) -> Result<(Value, Value)> {
        if self.is_nullable(ty) {
            let tag = self.load_i32(value, 0);
            let payload = self.op(
                Operator::F64Load {
                    memory: self.memory(8),
                },
                &[value],
                CoreType::F64,
            );
            return Ok((tag, payload));
        }
        let tag = self.integer(self.tag(ty)? as u32);
        Ok((
            tag,
            abi::encode_payload(&mut self.body, self.block, Some(value)),
        ))
    }

    pub(super) fn box_value(&mut self, ty: Type, value: Value) -> Result<Value> {
        if self.is_nullable(ty) {
            return Ok(value);
        }
        let (tag, payload) = self.tagged(ty, value)?;
        Ok(self.call(self.registry.value_helpers.unwrap().new, &[tag, payload]))
    }

    pub(super) fn extract(&mut self, ty: Type, boxed: Value) -> Result<Value> {
        if self.is_nullable(ty) {
            return Ok(boxed);
        }
        let tag = self.integer(self.tag(ty)? as u32);
        let payload =
            self.call_checked(self.registry.value_helpers.unwrap().extract, &[boxed, tag]);
        self.decode(ty, payload)
    }

    pub(super) fn decode(&mut self, ty: Type, payload: Value) -> Result<Value> {
        let reference = self.is_nullable(ty) || self.tag(ty)? as u32 != ValueTag::Number as u32;
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            reference,
        ))
    }

    pub(super) fn set_field(
        &mut self,
        object: Value,
        key: &str,
        ty: Type,
        value: Value,
    ) -> Result<()> {
        let key = self.integer(self.strings.get(key).unwrap());
        let (tag, payload) = self.tagged(ty, value)?;
        self.call_checked(
            self.registry.object_helpers.unwrap().set,
            &[object, key, tag, payload],
        );
        Ok(())
    }

    pub(super) fn read_field(&mut self, object: Value, key: &str, ty: Type) -> Result<Value> {
        if self.is_nullable(ty) {
            let key = self.integer(self.strings.get(key).unwrap());
            let entry = self.call(self.registry.object_helpers.unwrap().get, &[object, key]);
            return Ok(self.call(self.registry.object_helpers.unwrap().dynamic, &[entry]));
        }
        Ok(self.field(object, key, self.tag(ty)?))
    }

    pub(super) fn flat_types(&self, ty: Type) -> Result<Vec<CoreType>> {
        let mut storage = [WasmType::I32; 17];
        let mut flat = FlatTypes::new(&mut storage);
        ensure!(
            self.wit.resolve.push_flat(&ty, &mut flat),
            "Value requires indirect canonical parameters"
        );
        Ok(flat
            .to_vec()
            .into_iter()
            .map(super::super::core_type)
            .collect())
    }

    pub(super) fn convert_flat(&mut self, value: Value, target: CoreType) -> Result<Value> {
        let source = self.body.values[value].ty(&self.body.type_pool).unwrap();
        if source == target {
            return Ok(value);
        }
        let integer = match source {
            CoreType::F32 => self.op(Operator::I32ReinterpretF32, &[value], CoreType::I32),
            CoreType::F64 => self.op(Operator::I64ReinterpretF64, &[value], CoreType::I64),
            _ => value,
        };
        let source = self.body.values[integer].ty(&self.body.type_pool).unwrap();
        let width = if matches!(target, CoreType::F64 | CoreType::I64) {
            CoreType::I64
        } else {
            CoreType::I32
        };
        let integer = match (source, width) {
            (CoreType::I32, CoreType::I64) => self.op(Operator::I64ExtendI32U, &[integer], width),
            (CoreType::I64, CoreType::I32) => self.op(Operator::I32WrapI64, &[integer], width),
            _ => integer,
        };
        Ok(match target {
            CoreType::F32 => self.op(Operator::F32ReinterpretI32, &[integer], target),
            CoreType::F64 => self.op(Operator::F64ReinterpretI64, &[integer], target),
            CoreType::I32 | CoreType::I64 => integer,
            _ => bail!("Unsupported canonical flat type: {target:?}"),
        })
    }

    fn case_blocks(&mut self, discriminant: Value, count: usize) -> Vec<Block> {
        let mut cases = Vec::new();
        for index in 0..count {
            let index = self.integer(index as u32);
            let equal = self.op(Operator::I32Eq, &[discriminant, index], CoreType::I32);
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
            cases.push(selected);
            self.block = next;
        }
        self.body
            .set_terminator(self.block, Terminator::Unreachable);
        cases
    }

    pub(super) fn lift_variant(
        &mut self,
        ty: Type,
        shape: VariantShape,
        input: &mut Input<'_>,
    ) -> Result<Value> {
        let discriminant = self.read_scalar(tag_type(shape.tag), input)?;
        let mut payload = match input {
            Input::Flat { values, cursor } => {
                let count = self.flat_types(ty)?.len() - 1;
                let payload = Input::Flat {
                    values: &values[*cursor..*cursor + count],
                    cursor: 0,
                };
                *cursor += count;
                payload
            }
            Input::Memory { pointer, offset } => Input::Memory {
                pointer: *pointer,
                offset: *offset + shape.payload_offset,
            },
        };
        let start = match &payload {
            Input::Memory { offset, .. } => *offset,
            _ => 0,
        };
        let blocks = self.case_blocks(discriminant, shape.cases.len());
        let join = self.body.add_block();
        let output = self.body.add_blockparam(join, CoreType::I32);
        for (index, ((name, ty), block)) in shape.cases.iter().zip(blocks).enumerate() {
            self.block = block;
            match &mut payload {
                Input::Flat { cursor, .. } => *cursor = 0,
                Input::Memory { offset, .. } => *offset = start,
            }
            let child = ty.map(|ty| self.lift(ty, &mut payload)).transpose()?;
            let value = match shape.kind {
                VariantKind::Option => match (ty, child) {
                    (Some(ty), Some(child)) => self.box_value(*ty, child)?,
                    _ => {
                        let tag = self.integer(ValueTag::Undefined as u32);
                        let zero = self.number(0.);
                        self.call(self.registry.value_helpers.unwrap().new, &[tag, zero])
                    }
                },
                VariantKind::Result | VariantKind::Named => {
                    let object = self.call(self.registry.object_helpers.unwrap().new, &[]);
                    let (field, field_type, label) = if matches!(shape.kind, VariantKind::Result) {
                        ("ok", Type::Bool, self.integer(u32::from(index == 0)))
                    } else {
                        (
                            "tag",
                            Type::String,
                            self.integer(self.strings.get(name).unwrap()),
                        )
                    };
                    self.set_field(object, field, field_type, label)?;
                    if let (Some(ty), Some(child)) = (ty, child) {
                        self.set_field(
                            object,
                            if matches!(shape.kind, VariantKind::Result) {
                                name
                            } else {
                                "val"
                            },
                            *ty,
                            child,
                        )?;
                    }
                    object
                }
            };
            self.body.set_terminator(
                self.block,
                Terminator::Br {
                    target: BlockTarget {
                        block: join,
                        args: vec![value],
                    },
                },
            );
        }
        self.block = join;
        Ok(output)
    }

    pub(super) fn lower_variant(
        &mut self,
        shape: VariantShape,
        value: Value,
        pointer: Value,
        offset: u32,
    ) -> Result<()> {
        let discriminant = match shape.kind {
            VariantKind::Option => {
                let tag = self.load_i32(value, 0);
                let null = self.integer(ValueTag::Null as u32);
                self.op(Operator::I32GtU, &[tag, null], CoreType::I32)
            }
            VariantKind::Result => {
                let ok = self.field(value, "ok", ValueTag::Boolean);
                let one = self.integer(1);
                self.op(Operator::I32Sub, &[one, ok], CoreType::I32)
            }
            VariantKind::Named => {
                let label = self.field(value, "tag", ValueTag::String);
                let join = self.body.add_block();
                let output = self.body.add_blockparam(join, CoreType::I32);
                for (index, (name, _)) in shape.cases.iter().enumerate() {
                    let expected = self.integer(self.strings.get(name).unwrap());
                    let comparison = self.call(
                        self.registry.string_helpers.unwrap().str_compare,
                        &[label, expected],
                    );
                    let equal = self.op(Operator::I32Eqz, &[comparison], CoreType::I32);
                    let yes = self.body.add_block();
                    let no = self.body.add_block();
                    self.body.set_terminator(
                        self.block,
                        Terminator::CondBr {
                            cond: equal,
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
                    self.block = yes;
                    let index = self.integer(index as u32);
                    self.body.set_terminator(
                        yes,
                        Terminator::Br {
                            target: BlockTarget {
                                block: join,
                                args: vec![index],
                            },
                        },
                    );
                    self.block = no;
                }
                self.body
                    .set_terminator(self.block, Terminator::Unreachable);
                self.block = join;
                output
            }
        };
        let memory = self.memory(offset);
        let operator = match shape.tag {
            Int::U8 => Operator::I32Store8 { memory },
            Int::U16 => Operator::I32Store16 { memory },
            _ => Operator::I32Store { memory },
        };
        self.body
            .add_op(self.block, operator, &[pointer, discriminant], &[]);
        let blocks = self.case_blocks(discriminant, shape.cases.len());
        let join = self.body.add_block();
        for ((name, ty), block) in shape.cases.iter().zip(blocks) {
            self.block = block;
            if let Some(ty) = ty {
                let child = match shape.kind {
                    VariantKind::Option => self.extract(*ty, value)?,
                    VariantKind::Result => self.read_field(value, name, *ty)?,
                    VariantKind::Named => self.read_field(value, "val", *ty)?,
                };
                self.lower(*ty, child, pointer, offset + shape.payload_offset)?;
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
        Ok(())
    }

    pub(super) fn flatten_variant(
        &mut self,
        ty: Type,
        shape: VariantShape,
        pointer: Value,
        offset: u32,
        values: &mut Vec<Value>,
    ) -> Result<()> {
        let discriminant = self.load_scalar(tag_type(shape.tag), pointer, offset)?;
        let types = self.flat_types(ty)?;
        let blocks = self.case_blocks(discriminant, shape.cases.len());
        let join = self.body.add_block();
        let output: Vec<_> = types
            .iter()
            .map(|ty| self.body.add_blockparam(join, *ty))
            .collect();
        for ((_, ty), block) in shape.cases.iter().zip(blocks) {
            self.block = block;
            let mut flattened = vec![discriminant];
            if let Some(ty) = ty {
                self.flatten_memory(*ty, pointer, offset + shape.payload_offset, &mut flattened)?;
            }
            let mut args = Vec::new();
            for (index, ty) in types.iter().enumerate() {
                let value = if let Some(value) = flattened.get(index) {
                    *value
                } else {
                    self.integer(0)
                };
                args.push(self.convert_flat(value, *ty)?);
            }
            self.body.set_terminator(
                self.block,
                Terminator::Br {
                    target: BlockTarget { block: join, args },
                },
            );
        }
        self.block = join;
        values.extend(output);
        Ok(())
    }
}

fn tag_type(tag: Int) -> Type {
    match tag {
        Int::U8 => Type::U8,
        Int::U16 => Type::U16,
        _ => Type::U32,
    }
}
