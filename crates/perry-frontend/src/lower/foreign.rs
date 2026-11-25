//! Imported async functions specialize only their canonical input views.
use super::*;
use perry_mir::{ForeignOp, Phase};

/// Whether `actual` is structurally compatible with outbound import parameter type `expected`.
pub(super) fn accepts_input(ctx: &Context, expected: TypeHandle, actual: TypeHandle) -> bool {
    fn accepts(
        ctx: &Context,
        expected: TypeHandle,
        actual: TypeHandle,
        seen: &mut HashSet<(TypeHandle, TypeHandle)>,
    ) -> bool {
        if expected == actual || !seen.insert((expected, actual)) {
            return true;
        }
        let expected = expected.deref(ctx);
        let actual = actual.deref(ctx);
        if expected.is::<perry_mir::ByteInputType>() {
            return actual.is::<BytesType>() || actual.is::<StringType>();
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<perry_mir::OptionalType>(),
            actual.downcast_ref::<perry_mir::OptionalType>(),
        ) {
            return (!b.absence().admits_null() || a.absence().admits_null())
                && (!b.absence().admits_undefined() || a.absence().admits_undefined())
                && accepts(ctx, a.value(), b.value(), seen);
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<ArrayType>(),
            actual.downcast_ref::<ArrayType>(),
        ) {
            return accepts(ctx, a.element(), b.element(), seen);
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<RecordType>(),
            actual.downcast_ref::<RecordType>(),
        ) {
            let (Ok(a), Ok(b)) = (a.fields(), b.fields()) else {
                return false;
            };
            return a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((an, at), (bn, bt))| an == bn && accepts(ctx, *at, *bt, seen));
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<perry_mir::VariantType>(),
            actual.downcast_ref::<perry_mir::VariantType>(),
        ) {
            return a.cases().len() == b.cases().len()
                && a.cases().iter().zip(b.cases()).all(|((an, at), (bn, bt))| {
                    an == bn
                        && match (at, bt) {
                            (Some(a), Some(b)) => accepts(ctx, *a, *b, seen),
                            (None, None) => true,
                            _ => false,
                        }
                });
        }
        false
    }
    accepts(ctx, expected, actual, &mut HashSet::new())
}

pub(super) fn wrapper<'h>(
    ctx: &mut Context,
    shapes: &Shapes<'h>,
    binding: &'h str,
    params: Vec<TypeHandle>,
    result: TypeHandle,
) -> Result<Identifier> {
    let key = (binding, params.clone());
    if let Some(name) = shapes.foreign_wrappers.borrow().get(&key) {
        return Ok(name.clone());
    }
    let mut index = shapes.foreign_wrappers.borrow().len();
    let name = loop {
        let name = format!("perry_host_wrapper_{index}");
        if !shapes.has_function(&name)
            && shapes
                .foreign_wrappers
                .borrow()
                .values()
                .all(|other| other.as_ref() != name)
        {
            break identifier(&name)?;
        }
        index += 1;
    };
    let ty = FunctionType::get(ctx, params, vec![result]);
    let function = FuncOp::new(ctx, name.clone(), ty);
    set_async(ctx, &function);
    let block = function.get_entry_block(ctx);
    let mut inserter = IRInserter::<DummyListener>::new_at_block_end(block);
    let args = block.deref(ctx).arguments().collect();
    let transfer = perry_mir::TransferType::get(ctx).into();
    let promise = PromiseType::get(ctx, transfer).into();
    let start = ForeignOp::new(ctx, Phase::Start, binding, args, vec![promise]);
    inserter.append_op(ctx, &start);
    let pending = start.get_operation().deref(ctx).get_result(0);
    let await_op = AwaitOp::new(ctx, pending).to_diag(ctx)?;
    inserter.append_op(ctx, &await_op);
    let finish = ForeignOp::new(
        ctx,
        Phase::Finish,
        binding,
        vec![await_op.get_result(ctx)],
        vec![result],
    );
    inserter.append_op(ctx, &finish);
    let value = finish.get_operation().deref(ctx).get_result(0);
    let ret = ReturnOp::new(ctx, vec![value]);
    inserter.append_op(ctx, &ret);
    shapes
        .module
        .append_operation(ctx, function.get_operation(), 0);
    shapes
        .foreign_wrappers
        .borrow_mut()
        .insert(key, name.clone());
    Ok(name)
}

impl crate::wit::WitValue<'_> {
    pub(super) fn mir_type(self, ctx: &Context, input: bool) -> Result<TypeHandle> {
        use perry_contract::types::{Contract, Input, convert};
        use perry_mir::{OptionalType, ResourceType, VariantType};
        use wit_parser::Type;

        // A field or tuple slot preserves the exact bit pattern of an integer64.
        let field = |binding: BindingType| {
            binding
                .integer
                .filter(|integer| integer.deref(ctx).width() == 64)
                .map_or(binding.carrier, Into::into)
        };
        let binding = convert(
            self.resolve,
            Input::Reference(self.ty),
            &|_| None,
            &|contract: Contract<'_, Result<BindingType>>| {
                Ok(BindingType::plain(match contract {
                    Contract::Scalar(ty) => {
                        if let Some((width, sign)) = (Self { ty, ..self }).integer() {
                            return Ok(BindingType {
                                carrier: if width == 64 {
                                    BigIntType::get(ctx).into()
                                } else {
                                    FP64Type::get(ctx).into()
                                },
                                integer: Some(IntegerType::get(ctx, width, sign)),
                            });
                        }
                        match ty {
                            Type::Bool => IntegerType::get(ctx, 1, Signedness::Signless).into(),
                            Type::F32 | Type::F64 => FP64Type::get(ctx).into(),
                            Type::String | Type::Char => StringType::get(ctx).into(),
                            _ => bail_unsupported!("unsupported WIT scalar {ty:?}"),
                        }
                    }
                    Contract::Resource { id, .. } => ResourceType::get(
                        ctx,
                        perry_contract::names::canonical_type(self.resolve, id).index() as u32,
                    )
                    .into(),
                    Contract::Bytes if input => perry_mir::ByteInputType::get(ctx).into(),
                    Contract::Bytes => BytesType::get(ctx).into(),
                    Contract::ByteStream => perry_mir::ReadableType::get(ctx).into(),
                    Contract::List(element) => ArrayType::get(ctx, element?.carrier).into(),
                    Contract::Option(value) => crate::lower::coerce::admitting(
                        ctx,
                        value?.carrier,
                        perry_mir::Absence::Null,
                    ),
                    Contract::Enum(_) => StringType::get(ctx).into(),
                    Contract::Record(fields) => {
                        let fields = fields
                            .map(|(name, b)| Ok((name, field(b?))))
                            .collect::<Result<_>>()?;
                        RecordType::structural(ctx, RecordKind::Object, fields)
                            .to_diag(ctx)?
                            .into()
                    }
                    Contract::Tuple(elements) => {
                        let fields = elements
                            .enumerate()
                            .map(|(i, b)| Ok((format!("_{i}"), field(b?))))
                            .collect::<Result<_>>()?;
                        RecordType::structural(ctx, RecordKind::Tuple, fields)
                            .to_diag(ctx)?
                            .into()
                    }
                    Contract::Variant(cases) => {
                        let cases = cases
                            .map(|(tag, value)| {
                                Ok((tag.into(), value.transpose()?.map(|b| b.carrier)))
                            })
                            .collect::<Result<_>>()?;
                        VariantType::get(ctx, cases).into()
                    }
                    Contract::Result { ok, err } => {
                        let mut fields = vec![(
                            "ok".into(),
                            IntegerType::get(ctx, 1, Signedness::Signless).into(),
                        )];
                        for (name, value) in [("value", ok), ("error", err)] {
                            if let Some(value) = value {
                                fields.push((
                                    name.into(),
                                    OptionalType::get(ctx, value?.carrier).into(),
                                ));
                            }
                        }
                        RecordType::structural(ctx, RecordKind::Object, fields)
                            .to_diag(ctx)?
                            .into()
                    }
                    Contract::Unsupported(kind) => bail_unsupported!("unsupported WIT type {kind}"),
                }))
            },
        )?;
        Ok(binding.carrier)
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn import_call(
        &mut self,
        ctx: &mut Context,
        binding: &'s str,
        signature: &'s crate::wit::ImportSignature<'s>,
        args: &'s [Expr],
        byte_offset: u32,
        resource_method: bool,
    ) -> Result<Option<Value>> {
        if args.len() != signature.params.len() {
            let error = diag!(
                codes::CONTRACT_MISMATCH,
                "WIT import {binding} expects {} arguments, got {}",
                signature.params.len(),
                args.len()
            );
            return Err(self.shapes.annotate_call(error, byte_offset));
        }
        let mut values = Vec::new();
        for (arg, &wit) in args.iter().zip(&signature.params) {
            let ty = wit.mir_type(ctx, true)?;
            let value = if resource_method && let Some(result) = wit.result() {
                self.resource_input(ctx, arg, wit, result, ty)?
            } else if resource_method
                && matches!(arg, Expr::Undefined)
                && absence(ctx, ty).is_some_and(|states| states.admits_null())
            {
                self.absent_literal(ctx, true, Some(ty))?
            } else {
                self.expression_with(ctx, arg, Some(ty))?
            };
            let actual = value.get_type(ctx);
            let value = if foreign::accepts_input(ctx, ty, actual) {
                value
            } else if optional_value(ctx, ty).is_none()
                && optional_value(ctx, actual)
                    .is_some_and(|inner| foreign::accepts_input(ctx, ty, inner))
            {
                self.defined(ctx, value)?
            } else if optional_value(ctx, actual).is_none()
                && let Some((inner, states)) = optional_parts(ctx, ty)
                && foreign::accepts_input(ctx, inner, actual)
            {
                let wrapped = perry_mir::OptionalType::get_with(ctx, actual, states).into();
                self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, wrapped))
            } else {
                self.fit(ctx, value, ty)?
            };
            values.push(value);
        }
        let result = signature
            .result
            .map(|ty| ty.mir_type(ctx, false))
            .transpose()?;
        if signature.is_async {
            let payload = result.unwrap_or_else(|| void_type(ctx));
            let params = values.iter().map(|v| v.get_type(ctx)).collect();
            let wrapper = foreign::wrapper(ctx, self.shapes, binding, params, payload)?;
            let op = AsyncCallOp::new(ctx, wrapper, values, payload);
            return Ok(Some(self.append_value(ctx, &op)));
        }
        let op = perry_mir::ForeignOp::new(
            ctx,
            perry_mir::Phase::Call,
            binding,
            values,
            result.into_iter().collect(),
        );
        self.append(ctx, &op);
        let raw = op.get_operation().deref(ctx).results().next();
        if resource_method
            && let (Some(wit), Some(value)) = (signature.result, raw)
            && let Some(result) = wit.result()
        {
            return self.resource_result(ctx, value, wit, result);
        }
        Ok(raw)
    }
}

impl<'s> FunctionBuilder<'s> {
    fn resource_input(
        &mut self,
        ctx: &mut Context,
        arg: &'s Expr,
        wit: crate::wit::WitValue<'_>,
        result: &wit_parser::Result_,
        raw: TypeHandle,
    ) -> Result<Value> {
        let payload = |ty| (crate::wit::WitValue { ty, ..wit }).mir_type(ctx, false);
        let ok = result.ok.map(payload).transpose()?;
        let err = result.err.map(payload).transpose()?;
        let variant =
            perry_mir::VariantType::get(ctx, vec![("ok".into(), ok), ("err".into(), err)]).into();
        let value = self.expression_with(ctx, arg, Some(variant))?;
        let value = self.fit(ctx, value, variant)?;
        let tag = self.emit_value(ctx, |ctx| perry_mir::VariantTagOp::new(ctx, value));
        let ok_tag = self.string(ctx, "ok");
        let test = self.compare(ctx, CompareKind::Eq, tag, ok_tag);
        let record = perry_mir::typed::<RecordType>(ctx, raw)
            .ok_or_else(|| internal!("WIT result requires a record"))?;
        let build = |builder: &mut Self,
                     ctx: &mut Context,
                     success,
                     active: Option<TypeHandle>|
         -> Result<Value> {
            let active_field = if success { "value" } else { "error" };
            let fields = record.deref(ctx).fields().to_diag(ctx)?.to_vec();
            let mut values = Vec::with_capacity(fields.len());
            for (name, ty) in fields {
                values.push(if name == "ok" {
                    builder.boolean(ctx, success)
                } else if name == active_field {
                    let payload =
                        active.ok_or_else(|| internal!("missing WIT result payload type"))?;
                    let content = builder.emit_value(ctx, |ctx| {
                        perry_mir::VariantPayloadOp::new(ctx, value, payload)
                    });
                    builder.fit(ctx, content, ty)?
                } else {
                    builder.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty))
                });
            }
            Ok(builder.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)))
        };
        self.choose(
            ctx,
            raw,
            test,
            |b, ctx| build(b, ctx, true, ok),
            |b, ctx| build(b, ctx, false, err),
        )
    }

    fn resource_result(
        &mut self,
        ctx: &mut Context,
        raw: Value,
        wit: crate::wit::WitValue<'_>,
        result: &wit_parser::Result_,
    ) -> Result<Option<Value>> {
        let ok = self.record_field(ctx, raw, "ok")?;
        let success = self.new_block(ctx, "resource_ok");
        let failure = self.new_block(ctx, "resource_error");
        self.branch(ctx, ok, success, failure);
        self.switch_to(failure);
        if let Some(ty) = result.err {
            let error = self.record_field(ctx, raw, "error")?;
            let variant = match ty {
                wit_parser::Type::Id(id) => match &wit.resolve.types
                    [perry_contract::names::canonical_type(wit.resolve, id)]
                .kind
                {
                    wit_parser::TypeDefKind::Variant(variant) => Some(variant),
                    _ => None,
                },
                _ => None,
            };
            let error = if let Some(variant) = variant {
                let error = self.defined(ctx, error)?;
                let tag = self.emit_value(ctx, |ctx| perry_mir::VariantTagOp::new(ctx, error));
                for case in &variant.cases {
                    let Some(owner) = case
                        .ty
                        .and_then(|ty| crate::wit::owned_resource(wit.resolve, ty))
                    else {
                        continue;
                    };
                    let drop = self
                        .shapes
                        .metadata
                        .resource_members
                        .iter()
                        .find(|member| {
                            member.owner == owner.index() as u32
                                && member.kind == crate::wit::MemberKind::Dispose
                        })
                        .ok_or_else(|| internal!("owned WIT error has no resource destructor"))?;
                    let expected = self.string(ctx, &case.name);
                    let matches = self.compare(ctx, CompareKind::Eq, tag, expected);
                    let release = self.new_block(ctx, "resource_drop");
                    let next = self.new_block(ctx, "resource_drop_done");
                    self.branch(ctx, matches, release, next);
                    self.switch_to(release);
                    let resource = perry_mir::ResourceType::get(ctx, owner.index() as u32).into();
                    let handle = self.emit_value(ctx, |ctx| {
                        perry_mir::VariantPayloadOp::new(ctx, error, resource)
                    });
                    self.emit(ctx, |ctx| {
                        ForeignOp::new(ctx, Phase::Call, &drop.binding, vec![handle], vec![])
                    });
                    self.jump(ctx, next);
                    self.switch_to(next);
                }
                self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, ErrorKind::Any, tag))
            } else {
                error
            };
            self.throw_value(ctx, error)?;
        } else {
            self.raise_error(ctx, ErrorKind::Any, "");
        }
        self.switch_to(success);
        result
            .ok
            .map(|ty| {
                let expected = (crate::wit::WitValue { ty, ..wit }).mir_type(ctx, false)?;
                let value = self.record_field(ctx, raw, "value")?;
                self.fit(ctx, value, expected)
            })
            .transpose()
    }
}
