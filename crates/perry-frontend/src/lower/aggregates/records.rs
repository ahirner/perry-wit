//! Ordered record assembly preserves field presence independently of storage order.
use super::*;
use perry_hir::ir::ObjectPart;
use std::{
    borrow::Cow,
    iter::{empty, once},
};

enum RecordSource<'a> {
    Spread(Value),
    Insert(&'a str),
    Exclude(&'a str),
}

#[derive(Clone, Copy)]
enum CopyMode {
    Spread,
    Projection,
}

enum RecordInput<'s> {
    Expression(&'s Expr, Option<TypedHandle<RecordType>>),
    Field(Value, CopyMode),
}

struct RecordAssembly<'a> {
    expected: Option<TypedHandle<RecordType>>,
    fields: Vec<(Cow<'a, str>, Value)>,
}

enum MissingFields {
    Diagnose,
    Infer,
}

fn matching_record<'a>(
    ctx: &Context,
    record: Option<TypedHandle<RecordType>>,
    mut names: impl Iterator<Item = &'a str> + Clone,
    missing: MissingFields,
) -> Result<Option<TypedHandle<RecordType>>> {
    let Some(record) = record else {
        return Ok(None);
    };
    let definition = record.deref(ctx);
    let fields = definition
        .fields()
        .map_err(|_| Diagnostic::internal("record without fields"))?;
    let required = fields
        .iter()
        .filter(|(name, ty)| !admits_undefined(ctx, *ty) && !names.clone().any(|key| key == name));
    match missing {
        MissingFields::Diagnose => {
            let missing: Vec<_> = required.map(|(name, _)| name.as_str()).collect();
            if !missing.is_empty() {
                bail_diag!(
                    codes::MISSING_REQUIRED_FIELD,
                    "missing required record fields: {}",
                    missing.join(", ")
                );
            }
        }
        MissingFields::Infer if required.count() != 0 => return Ok(None),
        MissingFields::Infer => {}
    }
    Ok(names
        .all(|name| definition.field(name).is_some())
        .then_some(record))
}

fn merged_field_type(ctx: &Context, previous: TypeHandle, incoming: TypeHandle) -> TypeHandle {
    perry_mir::Absence::join_optional(absence(ctx, previous), absence(ctx, incoming)).map_or(
        incoming,
        |states| {
            let inner = optional_value(ctx, incoming)
                .or_else(|| optional_value(ctx, previous))
                .unwrap_or(incoming);
            perry_mir::OptionalType::get_with(ctx, inner, states).into()
        },
    )
}

impl<'a> RecordAssembly<'a> {
    fn write<'s>(
        &mut self,
        b: &mut FunctionBuilder<'s>,
        ctx: &mut Context,
        name: Cow<'a, str>,
        input: RecordInput<'s>,
    ) -> Result<()> {
        let expected = self
            .expected
            .and_then(|record| record.deref(ctx).field(&name).map(|(_, ty)| ty));
        let position = self.fields.iter().position(|(key, _)| key == &name);
        let previous = position.map(|index| self.fields[index].1);
        let (mut loaded, source, merge) = match input {
            RecordInput::Expression(expr, fallback) => {
                let absent = match expr {
                    Expr::Null => Some(perry_mir::Absence::Null),
                    Expr::Undefined => Some(perry_mir::Absence::Undefined),
                    _ => None,
                };
                if self.expected.is_some()
                    && expected.is_none()
                    && absent == Some(perry_mir::Absence::Undefined)
                {
                    if let Some(position) = position {
                        self.fields.remove(position);
                    }
                    return Ok(());
                }
                let contextual = match (expected, absent) {
                    (None, Some(absent)) => previous
                        .map(|value| value.get_type(ctx))
                        .or_else(|| {
                            fallback
                                .and_then(|record| record.deref(ctx).field(&name).map(|(_, ty)| ty))
                        })
                        .map(|ty| crate::lower::coerce::admitting(ctx, ty, absent)),
                    _ => expected,
                };
                (
                    b.expression_with(ctx, expr, contextual)?,
                    None,
                    self.expected.is_none(),
                )
            }
            RecordInput::Field(source, mode) => {
                let op = FieldGetOp::new(ctx, source, name.to_string()).to_diag(ctx)?;
                let value = b.field_read(ctx, &op)?;
                let presence = admits_undefined(ctx, value.get_type(ctx)).then_some((source, mode));
                (value, presence, presence.is_some())
            }
        };
        let loaded_ty = loaded.get_type(ctx);
        let target = expected.unwrap_or_else(|| {
            previous.filter(|_| merge).map_or(loaded_ty, |value| {
                merged_field_type(ctx, value.get_type(ctx), loaded_ty)
            })
        });
        let field_note = |err: Diagnostic| err.with_note(format!("in record field '{name}'"));
        // Spread fits first copies eagerly; projection converts only present fields.
        if previous.is_none() && !matches!(source, Some((_, CopyMode::Projection))) {
            loaded = b.fit(ctx, loaded, target).map_err(field_note)?;
        }
        let optional = admits_undefined(ctx, target);
        let value = if let Some((source, _)) = source.filter(|_| previous.is_some() || optional) {
            let present = if optional {
                let op = FieldPresentOp::new(ctx, source, name.to_string()).to_diag(ctx)?;
                b.emit_value(ctx, |_| op)
            } else {
                let undefined = b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, loaded));
                b.not(ctx, undefined)
            };
            b.choose(
                ctx,
                target,
                present,
                |b, ctx| {
                    let loaded = if optional {
                        loaded
                    } else {
                        b.strip_undefined(ctx, loaded)?
                    };
                    b.fit(ctx, loaded, target)
                },
                |b, ctx| match previous {
                    Some(value) => b.fit(ctx, value, target),
                    None => Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, target))),
                },
            )
        } else {
            b.fit(ctx, loaded, target)
        }
        .map_err(field_note)?;
        match position {
            Some(index) => self.fields[index].1 = value,
            None => self.fields.push((name, value)),
        }
        Ok(())
    }

    fn copy(
        &mut self,
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        source: Value,
        record: TypedHandle<RecordType>,
        excluded: &[String],
        mode: CopyMode,
    ) -> Result<()> {
        let count = record.deref(ctx).fields().to_diag(ctx)?.len();
        for index in 0..count {
            let name = {
                let definition = record.deref(ctx);
                let field = &definition.fields().to_diag(ctx)?[index];
                if excluded.contains(&field.0) {
                    continue;
                }
                field.0.clone()
            };
            self.write(b, ctx, Cow::Owned(name), RecordInput::Field(source, mode))?;
        }
        Ok(())
    }

    fn finish(
        self,
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        record: Option<TypedHandle<RecordType>>,
        sources: impl Iterator<Item = RecordSource<'a>>,
    ) -> Result<Value> {
        let mut sources = sources.peekable();
        let literal = sources.peek().is_none();
        let record = match record {
            Some(record) => record,
            None => b.shapes.mir_record(
                ctx,
                self.fields
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.get_type(ctx)))
                    .collect(),
            )?,
        };
        let count = record.deref(ctx).fields().to_diag(ctx)?.len();
        let mut slots = vec![None; count];
        let mut order = Vec::with_capacity(if literal { self.fields.len() } else { 0 });
        for (name, value) in self.fields {
            let (index, ty) = record
                .deref(ctx)
                .field(&name)
                .ok_or_else(|| diag!(codes::TYPE_MISMATCH, "record has no field {name}"))?;
            *slots
                .get_mut(index)
                .ok_or_else(|| internal!("record field slot {index} out of range"))? = Some(
                b.fit(ctx, value, ty)
                    .map_err(|err| err.with_note(format!("in record field '{name}'")))?,
            );
            if literal {
                order.push(index as u32);
            }
        }
        let values = slots
            .into_iter()
            .enumerate()
            .map(|(index, value)| match value {
                Some(value) => Ok(value),
                None => {
                    let ty = record.deref(ctx).fields().to_diag(ctx)?[index].1;
                    if !admits_undefined(ctx, ty) {
                        bail_diag!(
                            codes::MISSING_REQUIRED_FIELD,
                            "missing required record fields: {}",
                            record.deref(ctx).fields().to_diag(ctx)?[index].0
                        );
                    }
                    Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty)))
                }
            })
            .collect::<Result<_>>()?;
        let value = b.emit_value(ctx, |ctx| {
            let allocation = AllocOp::new(ctx, record, values);
            allocation.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
            allocation
        });
        for source in sources {
            let (intrinsic, operand) = match source {
                RecordSource::Spread(source) => (Intrinsic::JsonSpread, source),
                RecordSource::Insert(key) => (Intrinsic::JsonInsert, b.string(ctx, key)),
                RecordSource::Exclude(key) => (Intrinsic::JsonExclude, b.string(ctx, key)),
            };
            b.intrinsic_void(ctx, intrinsic, vec![value, operand]);
        }
        Ok(value)
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(in crate::lower) fn closure_value(
        &mut self,
        ctx: &mut Context,
        closure: &crate::engine::ClosureValue,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        use crate::engine::CaptureOperand;
        let record = self.shapes.environment_type(ctx, closure.identity)?;
        let record = perry_mir::typed::<RecordType>(ctx, record)
            .ok_or_else(|| internal!("capture environment has no record layout"))?;
        let layout = &self.shapes.specialization.instances.environments[&closure.identity];
        let mut assembly = RecordAssembly {
            expected: Some(record),
            fields: Vec::with_capacity(layout.len()),
        };
        if closure.operands.is_empty() {
            assembly
                .fields
                .push((Cow::Borrowed("_u"), self.number(ctx, 0.0)));
        } else {
            ensure_internal!(
                layout.len() == closure.operands.len(),
                "wrong number of closure captures"
            );
            for (field, &operand) in layout.iter().zip(&closure.operands) {
                let name = field.name();
                let value = match operand {
                    CaptureOperand::Local(id) => {
                        let value = self.load_local(ctx, id)?;
                        self.narrow_value(ctx, || Some(crate::narrowing::Path::local(id)), value)?
                    }
                    CaptureOperand::Cell(id) => self.load(ctx, self.local(id)?)?,
                    CaptureOperand::Function {
                        identity,
                        environment,
                    } => {
                        let payload = environment
                            .map(|id| {
                                let value = self.load_local(ctx, id)?;
                                let env = self.shapes.environment_type(ctx, identity)?;
                                self.callable_field(ctx, value, "val", env)
                            })
                            .transpose()?;
                        let (_, ty) = record
                            .deref(ctx)
                            .field(name)
                            .ok_or_else(|| internal!("unknown capture field {name}"))?;
                        self.function_value(ctx, identity, payload, Some(ty))?
                    }
                    CaptureOperand::Widen { local, from, to } => {
                        let value = self.load_local(ctx, local)?;
                        self.widen_function(ctx, value, from, to)?
                    }
                };
                assembly.fields.push((Cow::Borrowed(name), value));
            }
        }
        let env = assembly.finish(self, ctx, Some(record), empty())?;
        self.function_value(ctx, closure.identity, Some(env), expected)
    }

    pub(crate) fn record_literal(
        &mut self,
        ctx: &mut Context,
        fields: &[(&str, &'s Expr)],
        expected: Option<TypeHandle>,
        kind: RecordKind,
    ) -> Result<Value> {
        let expected = expected.map(|ty| present_type(ctx, ty));
        let expected = if let Some(ty) =
            expected.filter(|ty| ty.deref(ctx).is::<perry_mir::ValueUnionType>())
        {
            perry_mir::variant_cases(ctx, ty)
                .to_diag(ctx)?
                .into_iter()
                .find_map(|(tag, payload)| (tag == "object").then_some(payload).flatten())
        } else {
            expected
        };
        if kind == RecordKind::Object {
            if let Some(ty) = expected
                && ty.deref(ctx).is::<perry_mir::VariantType>()
            {
                return self.variant_literal(ctx, ty, fields);
            }
            if let Some(ty) = expected
                && let Some((_, value, tuple)) =
                    crate::lower::collections::dict_entry_types(ctx, ty)
            {
                return self.dict_literal(ctx, fields, value, tuple);
            }
        }
        let expected = expected
            .and_then(|ty| perry_mir::typed::<RecordType>(ctx, ty))
            .filter(|record| record.deref(ctx).kind() == kind);
        let expected = matching_record(
            ctx,
            expected,
            fields
                .iter()
                .filter(|(_, e)| !self.erased_operand(e))
                .map(|&(name, _)| name),
            MissingFields::Diagnose,
        )?;
        let mut assembly = RecordAssembly {
            expected,
            fields: Vec::with_capacity(fields.len()),
        };
        for &(name, arg) in fields {
            if self.erased_operand(arg) {
                self.discard_function_operand(ctx, arg)?;
                continue;
            }
            assembly.write(
                self,
                ctx,
                Cow::Borrowed(name),
                RecordInput::Expression(arg, None),
            )?;
        }
        assembly.finish(self, ctx, expected, empty())
    }

    pub(crate) fn record_object_spread(
        &mut self,
        ctx: &mut Context,
        parts: &'s [ObjectPart],
        first_evaluated: Option<Value>,
        expected: Option<TypedHandle<RecordType>>,
    ) -> Result<Value> {
        let fallback = expected.or_else(|| {
            first_evaluated
                .and_then(|value| {
                    perry_mir::typed::<RecordType>(ctx, present_type(ctx, value.get_type(ctx)))
                        .filter(|record| record.deref(ctx).kind() == RecordKind::Object)
                })
                .or_else(|| {
                    parts.iter().find_map(|part| match part {
                        ObjectPart::Spread(value) => self.peek_record_type(ctx, value),
                        _ => None,
                    })
                })
        });
        let mut assembly = RecordAssembly {
            expected,
            fields: Vec::new(),
        };
        let mut sources = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let expr = part.value();
            if let Some(key) = hir_compat::static_property(part) {
                sources.push(RecordSource::Insert(key));
                assembly.write(
                    self,
                    ctx,
                    Cow::Borrowed(key),
                    RecordInput::Expression(expr, fallback),
                )?;
            } else {
                let raw = if index == 0
                    && let Some(first) = first_evaluated
                {
                    first
                } else {
                    self.expression(ctx, expr)?
                };
                if matches!(kind(ctx, raw), Ok(Kind::Exception)) {
                    continue;
                }
                let source = self.defined(ctx, raw)?;
                let record = perry_mir::typed::<RecordType>(ctx, source.get_type(ctx))
                    .filter(|record| record.deref(ctx).kind() == RecordKind::Object)
                    .ok_or_else(|| Diagnostic::unsupported("object spread is supported over typed records or JSON values with static keys"))?;
                sources.push(RecordSource::Spread(source));
                assembly.copy(self, ctx, source, record, &[], CopyMode::Spread)?;
            }
        }
        let record = matching_record(
            ctx,
            expected,
            assembly.fields.iter().map(|(name, _)| name.as_ref()),
            MissingFields::Infer,
        )?;
        assembly.finish(self, ctx, record, sources.into_iter())
    }

    pub(crate) fn object_rest(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        exclude_keys: &[String],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let source = self.expression(ctx, object)?;
        let source = self.defined(ctx, source)?;
        let record = perry_mir::typed::<RecordType>(ctx, source.get_type(ctx))
            .filter(|record| record.deref(ctx).kind() == RecordKind::Object)
            .ok_or_else(|| {
                Diagnostic::unsupported("object rest destructuring requires a typed record")
            })?;
        let expected = expected
            .map(|ty| present_type(ctx, ty))
            .and_then(|ty| perry_mir::typed::<RecordType>(ctx, ty))
            .filter(|record| record.deref(ctx).kind() == RecordKind::Object);
        let mut assembly = RecordAssembly {
            expected,
            fields: Vec::new(),
        };
        assembly.copy(self, ctx, source, record, exclude_keys, CopyMode::Spread)?;
        let sources = once(RecordSource::Spread(source))
            .chain(exclude_keys.iter().map(|key| RecordSource::Exclude(key)));
        assembly.finish(self, ctx, expected, sources)
    }

    pub(crate) fn project_record(
        &mut self,
        ctx: &mut Context,
        value: Value,
        defined: bool,
        record: TypedHandle<RecordType>,
        target: TypeHandle,
    ) -> Result<Value> {
        let source = if defined {
            value
        } else {
            self.defined(ctx, value)?
        };
        let mut assembly = RecordAssembly {
            expected: Some(record),
            fields: Vec::new(),
        };
        assembly.copy(self, ctx, source, record, &[], CopyMode::Projection)?;
        let value = assembly.finish(self, ctx, Some(record), once(RecordSource::Spread(source)))?;
        self.fit(ctx, value, target)
    }
}
