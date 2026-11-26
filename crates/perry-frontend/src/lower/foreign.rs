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
    shapes: &mut Shapes<'h>,
    binding: &'h str,
    params: Vec<TypeHandle>,
    result: TypeHandle,
) -> Result<Identifier> {
    let key = (binding, params.clone());
    if let Some(name) = shapes.foreign_wrappers.get(&key) {
        return Ok(name.clone());
    }
    let mut index = shapes.foreign_wrappers.len();
    let name = loop {
        let name = format!("perry_host_wrapper_{index}");
        if !shapes.has_function(&name)
            && shapes
                .foreign_wrappers
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
    shapes.foreign_wrappers.insert(key, name.clone());
    Ok(name)
}

impl Shapes<'_> {
    fn wit_carrier(&self, value: crate::wit::WitValue<'_>) -> Result<TypeHandle> {
        Ok(self
            .resolved_binding(self.types.source.boundary_type(value)?)?
            .carrier)
    }

    fn import_input(
        &mut self,
        ctx: &Context,
        value: crate::wit::WitValue<'_>,
    ) -> Result<TypeHandle> {
        self.input_type(ctx, self.wit_carrier(value)?)
    }
}

pub(super) enum CallKind {
    Function,
    Resource(Option<Value>),
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn resource_call(
        &mut self,
        ctx: &mut Context,
        member: crate::wit::MemberId,
        receiver: Option<Value>,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let decls = self.shapes.decls;
        let binding = &decls.world.members[member].binding;
        let signature = decls.resource_signatures[member]
            .ok_or_else(|| internal!("missing WIT resource binding {binding}"))?;
        self.import_call(ctx, binding, signature, args, CallKind::Resource(receiver))
    }

    pub(super) fn import_call(
        &mut self,
        ctx: &mut Context,
        binding: &'s str,
        signature: &'s crate::wit::ImportSignature<'s>,
        args: &'s [Expr],
        kind: CallKind,
    ) -> Result<Option<Value>> {
        let mut params = signature.params.as_slice();
        let mut values = Vec::with_capacity(params.len());
        if let CallKind::Resource(Some(receiver)) = kind {
            let (this, rest) = params
                .split_first()
                .ok_or_else(|| internal!("WIT resource method has no receiver"))?;
            let ty = self.shapes.import_input(ctx, *this)?;
            values.push(self.fit(ctx, receiver, ty)?);
            params = rest;
        }
        if args.len() != params.len() {
            bail_diag!(
                codes::CONTRACT_MISMATCH,
                "WIT import {binding} expects {} arguments, got {}",
                params.len(),
                args.len()
            );
        }
        let resource_method = matches!(kind, CallKind::Resource(_));
        for (arg, &wit) in args.iter().zip(params) {
            let ty = self.shapes.import_input(ctx, wit)?;
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
            .map(|ty| self.shapes.wit_carrier(ty))
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

impl<'s> FunctionBuilder<'s, '_> {
    fn resource_input(
        &mut self,
        ctx: &mut Context,
        arg: &'s Expr,
        wit: crate::wit::WitValue<'_>,
        result: &wit_parser::Result_,
        raw: TypeHandle,
    ) -> Result<Value> {
        let mut payload = |ty| self.shapes.wit_carrier(crate::wit::WitValue { ty, ..wit });
        let ok = result.ok.map(&mut payload).transpose()?;
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
                    let world = self.shapes.decls.world;
                    let drop = world
                        .drops
                        .get(&(owner.index() as u32))
                        .copied()
                        .ok_or_else(|| internal!("owned WIT error has no resource destructor"))?;
                    let binding = &world.members[drop].binding;
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
                        ForeignOp::new(ctx, Phase::Call, binding, vec![handle], vec![])
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
                let expected = self
                    .shapes
                    .wit_carrier(crate::wit::WitValue { ty, ..wit })?;
                let value = self.record_field(ctx, raw, "value")?;
                self.fit(ctx, value, expected)
            })
            .transpose()
    }
}
