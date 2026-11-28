//! Imported async functions specialize only their canonical input views.
use super::*;
use crate::engine::types::WitView;
use perry_mir::{ForeignOp, Phase};

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
    fn wit_carrier(&self, value: crate::wit::WitValue<'_>, view: WitView) -> Result<TypeHandle> {
        Ok(self
            .resolved_binding(self.types.source.boundary_type(value, view)?)?
            .carrier)
    }

    fn resource_result_type(
        &self,
        ctx: &Context,
        wit: crate::wit::WitValue<'_>,
        result: &wit_parser::Result_,
    ) -> Result<TypeHandle> {
        let mut payload =
            |ty| self.wit_carrier(crate::wit::WitValue { ty, ..wit }, WitView::Boundary);
        Ok(perry_mir::VariantType::get(
            ctx,
            vec![
                ("ok".into(), result.ok.map(&mut payload).transpose()?),
                ("err".into(), result.err.map(payload).transpose()?),
            ],
        )
        .into())
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
        self.import_call(ctx, member, args, CallKind::Resource(receiver))
    }

    pub(super) fn import_call(
        &mut self,
        ctx: &mut Context,
        member: crate::wit::MemberId,
        args: &'s [Expr],
        kind: CallKind,
    ) -> Result<Option<Value>> {
        let (binding, signature) = member.binding(self.shapes.decls.world)?;
        let mut params = signature
            .abi_params
            .as_deref()
            .ok_or_else(|| internal!("missing WIT resource binding {binding}"))?;
        let mut values = Vec::with_capacity(params.len());
        if let CallKind::Resource(Some(receiver)) = kind {
            let (this, rest) = params
                .split_first()
                .ok_or_else(|| internal!("WIT resource method has no receiver"))?;
            let ty = self.shapes.wit_carrier(*this, WitView::Input)?;
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
            let ty = if resource_method && let Some(result) = wit.result() {
                self.shapes.resource_result_type(ctx, wit, result)?
            } else {
                self.shapes.wit_carrier(wit, WitView::Input)?
            };
            let value = if resource_method
                && matches!(arg, Expr::Undefined)
                && absence(ctx, ty).is_some_and(|states| states.admits_null())
            {
                self.absent_literal(ctx, true, Some(ty))?
            } else {
                self.expression_with(ctx, arg, Some(ty))?
            };
            let actual = value.get_type(ctx);
            let value = if perry_mir::accepts_import_input(ctx, actual, ty) {
                value
            } else if optional_value(ctx, ty).is_none()
                && optional_value(ctx, actual)
                    .is_some_and(|inner| perry_mir::accepts_import_input(ctx, inner, ty))
            {
                self.defined(ctx, value)?
            } else if optional_value(ctx, actual).is_none()
                && let Some((inner, states)) = optional_parts(ctx, ty)
                && perry_mir::accepts_import_input(ctx, actual, inner)
            {
                let wrapped = perry_mir::OptionalType::get_with(ctx, actual, states).into();
                self.emit_value(WrapOp::with_type(ctx, value, wrapped), ctx)
            } else {
                self.fit(ctx, value, ty)?
            };
            values.push(value);
        }
        let resource_result = signature
            .result
            .filter(|_| resource_method && !signature.is_async)
            .and_then(|wit| Some((wit, wit.result()?)));
        let result = match resource_result {
            Some((wit, result)) => Some(self.shapes.resource_result_type(ctx, wit, result)?),
            None => signature
                .result
                .map(|ty| self.shapes.wit_carrier(ty, WitView::Boundary))
                .transpose()?,
        };
        if signature.is_async {
            let payload = result.unwrap_or_else(|| void_type(ctx));
            let params = values.iter().map(|v| v.get_type(ctx)).collect();
            let wrapper = foreign::wrapper(ctx, self.shapes, binding, params, payload)?;
            let op = AsyncCallOp::new(ctx, wrapper, values, payload);
            return Ok(Some(self.emit_value(op, ctx)));
        }
        let op = perry_mir::ForeignOp::new(
            ctx,
            perry_mir::Phase::Call,
            binding,
            values,
            result.into_iter().collect(),
        );
        let op = self.emit(op, ctx);
        let raw = op.get_operation().deref(ctx).results().next();
        if let (Some((wit, result)), Some(value)) = (resource_result, raw) {
            return self.resource_result(ctx, value, wit, result);
        }
        Ok(raw)
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    fn resource_result(
        &mut self,
        ctx: &mut Context,
        raw: Value,
        wit: crate::wit::WitValue<'_>,
        result: &wit_parser::Result_,
    ) -> Result<Option<Value>> {
        let tag = self.emit_value(perry_mir::VariantTagOp::new(ctx, raw), ctx);
        let err_tag = self.string(ctx, "err");
        let failed = self.compare(ctx, CompareKind::Eq, tag, err_tag);
        let success = self.enter_case(ctx, failed);
        let payload = |b: &mut Self, ctx: &mut Context, ty| -> Result<Value> {
            let carrier = b
                .shapes
                .wit_carrier(crate::wit::WitValue { ty, ..wit }, WitView::Boundary)?;
            Ok(b.emit_value(perry_mir::VariantPayloadOp::new(ctx, raw, carrier), ctx))
        };
        if let Some(ty) = result.err {
            let error = payload(self, ctx, ty)?;
            let error = if let Some(wit_parser::TypeDefKind::Variant(variant)) =
                perry_contract::type_kind(wit.resolve, ty)
            {
                let tag = self.emit_value(perry_mir::VariantTagOp::new(ctx, error), ctx);
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
                    let (binding, _) = drop.binding(world)?;
                    let expected = self.string(ctx, &case.name);
                    let matches = self.compare(ctx, CompareKind::Eq, tag, expected);
                    let next = self.enter_case(ctx, matches);
                    let resource = perry_mir::ResourceType::get(ctx, owner.index() as u32).into();
                    let handle = self
                        .emit_value(perry_mir::VariantPayloadOp::new(ctx, error, resource), ctx);
                    self.emit(
                        ForeignOp::new(ctx, Phase::Call, binding, vec![handle], vec![]),
                        ctx,
                    );
                    self.jump(ctx, next);
                    self.switch_to(next);
                }
                self.emit_value(ErrorNewOp::new(ctx, ErrorKind::Any, tag), ctx)
            } else {
                error
            };
            self.throw_value(ctx, error)?;
        } else {
            self.raise_error(ctx, ErrorKind::Any, "");
        }
        self.switch_to(success);
        result.ok.map(|ty| payload(self, ctx, ty)).transpose()
    }
}
