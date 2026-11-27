//! MIR representations and operations for finite sets of function identities.
use super::*;
use crate::engine::solve::{CallTarget, OperandUse, SiteResolution, ValueCoercion};
use crate::engine::types::{FuncIdentity, Ty, func_tag};
use perry_mir::{VariantNewOp, VariantPayloadOp, VariantTagOp, VariantType};

#[derive(Clone, Copy)]
pub(in crate::lower) enum Callback<'s> {
    Source(&'s crate::engine::boundary::TemplateView<'s>),
    Function(&'s Signature<'s>, Option<Value>),
}

impl Callback<'_> {
    pub(super) fn first_parameter(self, shapes: &Shapes<'_>) -> Option<TypeHandle> {
        match self {
            Self::Function(signature, bound) => {
                let prefix = usize::from(bound.is_some());
                signature
                    .params
                    .get(prefix)
                    .filter(|p| p.position == prefix)
                    .map(|p| p.base.carrier)
            }
            Self::Source(decl) => shapes
                .resolved_binding(
                    shapes
                        .specialization
                        .instances
                        .local(decl.params.first()?.id)
                        .ok()?
                        .annotation,
                )
                .ok()
                .map(|ty| ty.carrier),
        }
    }
}

impl ValueCoercion {
    pub(super) fn emit<'s, 'm>(
        self,
        b: &mut FunctionBuilder<'s, 'm>,
        ctx: &mut Context,
        expected: Option<TypeHandle>,
        read: impl FnOnce(
            &mut FunctionBuilder<'s, 'm>,
            &mut Context,
            Option<TypeHandle>,
        ) -> Result<Value>,
    ) -> Result<Value> {
        match self {
            Self::Zero => Ok(b.number(ctx, 0.0)),
            Self::Tag(id, to) | Self::BoxEnv(id, to) => {
                let payload = if self.reads_operand() {
                    let value = read(b, ctx, None)?;
                    let env = b.shapes.environment(id)?.record.into();
                    Some(b.callable_field(ctx, value, "val", env)?)
                } else {
                    None
                };
                let target = match expected {
                    Some(ty) => ty,
                    None => b.shapes.resolved_binding(to)?.carrier,
                };
                b.function_value(ctx, id, payload, Some(target))
            }
            Self::Widen { from, to } => {
                let input = b.shapes.callable_type(from)?;
                let value = read(b, ctx, input)?;
                let value = match input {
                    Some(ty) => b.fit(ctx, value, ty)?,
                    None => value,
                };
                b.widen_function(ctx, value, from, to)
            }
        }
    }
}

impl CallTarget {
    pub(super) fn take_operand(self, args: &mut &[Value]) -> Result<Option<Value>> {
        if !self.needs_value() {
            return Ok(None);
        }
        let (&value, rest) = args
            .split_first()
            .ok_or_else(|| internal!("missing callback argument"))?;
        *args = rest;
        Ok(Some(value))
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn prepare_callback(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<Callback<'s>> {
        self.optional_callback(ctx, expr)?.ok_or_else(|| {
            unsupported!("array callbacks require an inline closure or named function")
        })
    }

    pub(in crate::lower) fn optional_callback(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<Option<Callback<'s>>> {
        let site = self.site(expr)?;
        let source = self.shapes.specialization;
        let callback = match (site.operand, site.resolution) {
            (Some(OperandUse::Callback(target)), _) => {
                let value = self
                    .instance_value(ctx, expr, None, site.resolution, true)?
                    .ok_or_else(|| diag!(codes::TYPE_MISMATCH, "a void call has no value"))?;
                let (signature, operand) = target.bind(self, ctx, Some(value))?;
                Callback::Function(signature, operand)
            }
            (_, Some(SiteResolution::Callback { target })) => {
                Callback::Function(self.signatures.solved.instance(target)?, None)
            }
            _ => {
                let id = match expr {
                    Expr::Closure { func_id, .. } => Some(*func_id),
                    _ => source
                        .instances
                        .reference_at(self.frame.owner, expr, site.resolution)?,
                };
                let Some(id) = id else { return Ok(None) };
                if let Some(signature) = self.signatures.named.get(&id) {
                    Callback::Function(signature, None)
                } else if let Some((declaration, _)) = source.closure(id) {
                    Callback::Source(declaration)
                } else {
                    return Err(internal!("unknown callback function {id}"));
                }
            }
        };
        Ok(Some(callback))
    }

    pub(super) fn call_values(
        &mut self,
        ctx: &mut Context,
        signature: &'s Signature<'s>,
        inputs: impl Iterator<Item = Value>,
    ) -> Result<Option<Value>> {
        self.invoke_arguments(ctx, signature, inputs, Arguments::Plain(&[]), None)
    }

    pub(super) fn dispatch_body(
        &mut self,
        ctx: &mut Context,
        receiver: bool,
        arms: crate::engine::types::DispatchArmSpan,
        args: &[Value],
    ) -> Result<()> {
        let plans = &self.shapes.specialization.instances;
        let arms = &plans.dispatch_arms[arms];
        let (&callee, args) = args
            .split_first()
            .ok_or_else(|| internal!("missing dispatcher callee"))?;
        let (receiver, args) = if receiver {
            let (&receiver, args) = args
                .split_first()
                .ok_or_else(|| internal!("missing dispatcher receiver"))?;
            (Some(receiver), args)
        } else {
            (None, args)
        };
        for null in [true, false] {
            if let Some(test) = self.sentinel_test(ctx, callee, null) {
                let next = self.enter_case(ctx, test);
                self.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
                self.switch_to(next);
            }
        }
        let callee = self.defined(ctx, callee)?;
        let tag = (arms.len() > 1)
            .then(|| self.callable_field(ctx, callee, "tag", StringType::get(ctx).into()))
            .transpose()?;
        let mut arms = arms.iter().peekable();
        while let Some(arm) = arms.next() {
            let next = if let Some(tag) = tag
                && arms.peek().is_some()
            {
                let label = self.string(ctx, &func_tag(arm.identity));
                let test = self.compare(ctx, CompareKind::Eq, tag, label);
                Some(self.enter_case(ctx, test))
            } else {
                None
            };
            let env = self
                .shapes
                .callable_environment(arm.identity)
                .map(|ty| self.callable_field(ctx, callee, "val", ty))
                .transpose()?;
            let inputs = env
                .into_iter()
                .chain(receiver.filter(|_| arm.receiver))
                .chain(args.iter().copied());
            let value =
                self.call_values(ctx, self.signatures.solved.instance(arm.target)?, inputs)?;
            self.return_callback(ctx, value, false)?;
            if let Some(next) = next {
                self.switch_to(next);
            }
        }
        Ok(())
    }

    pub(super) fn raise_non_callable(&mut self, ctx: &mut Context) -> Value {
        self.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
        let ty = void_type(ctx);
        let never = self.emit_value(UndefinedOp::with_type(ctx, ty), ctx);
        self.never_values.insert(never);
        never
    }

    pub(super) fn callable_typeof(
        &mut self,
        ctx: &mut Context,
        operand: &'s Expr,
        ty: Ty,
    ) -> Result<Value> {
        let value = self.callable_operand(ctx, operand, ty)?;
        let result = StringType::get(ctx).into();
        let done = self.result_block(ctx, result);
        for (null, label) in [(true, "object"), (false, "undefined")] {
            if let Some(test) = self.sentinel_test(ctx, value, null) {
                let label = self.string(ctx, label);
                self.yield_if(ctx, done, test, [label]);
            }
        }
        let label = self.string(ctx, "function");
        Ok(self.finish_result(ctx, done, label))
    }
    pub(super) fn compare_callables(
        &mut self,
        ctx: &mut Context,
        a: &'s Expr,
        b: &'s Expr,
        left_ty: Ty,
        right_ty: Ty,
    ) -> Result<Value> {
        let left = self.callable_operand(ctx, a, left_ty)?;
        let right = self.callable_operand(ctx, b, right_ty)?;
        let no = self.boolean(ctx, false);
        let done = self.result_block(ctx, no.get_type(ctx));
        for null in [true, false] {
            let left_absent = self.sentinel_test(ctx, left, null);
            let right_absent = self.sentinel_test(ctx, right, null);
            if let Some(test) = left_absent {
                self.yield_if(ctx, done, test, [right_absent.unwrap_or(no)]);
            }
            if let Some(test) = right_absent {
                self.yield_if(ctx, done, test, [no]);
            }
        }
        let types = self.shapes.types.source;
        let shared = types
            .func_members(&left_ty)
            .filter(|id| types.func_members(&right_ty).any(|other| other == *id))
            .collect::<Vec<_>>();
        if !shared.is_empty() {
            let left = self.defined(ctx, left)?;
            let right = self.defined(ctx, right)?;
            let string = StringType::get(ctx).into();
            let left_tag = self.callable_field(ctx, left, "tag", string)?;
            let right_tag = self.callable_field(ctx, right, "tag", string)?;
            let differ = self.compare(ctx, CompareKind::Ne, left_tag, right_tag);
            self.yield_if(ctx, done, differ, [no]);
            for identity in shared {
                let wanted = self.string(ctx, &func_tag(identity));
                let test = self.compare(ctx, CompareKind::Eq, left_tag, wanted);
                let next = self.enter_case(ctx, test);
                let equal = if let Some(env) = self.shapes.callable_environment(identity) {
                    let l = self.callable_field(ctx, left, "val", env)?;
                    let r = self.callable_field(ctx, right, "val", env)?;
                    self.compare(ctx, CompareKind::Eq, l, r)
                } else {
                    self.boolean(ctx, true)
                };
                self.emit(BrOp::new(ctx, done, vec![equal]), ctx);
                self.switch_to(next);
            }
        }
        Ok(self.finish_result(ctx, done, no))
    }

    fn widen_function(
        &mut self,
        ctx: &mut Context,
        value: Value,
        from: Ty,
        to: Ty,
    ) -> Result<Value> {
        let result = self
            .shapes
            .callable_type(to)?
            .ok_or_else(|| internal!("missing destination type for function union"))?;
        let done = self.result_block(ctx, result);
        for null in [true, false] {
            if let Some(test) = self.sentinel_test(ctx, value, null) {
                let absent = self.absent_literal(ctx, null, Some(result))?;
                self.yield_if(ctx, done, test, [absent]);
            }
        }
        let value = self.defined(ctx, value)?;
        let tag = self.callable_field(ctx, value, "tag", StringType::get(ctx).into())?;
        let mut members = self
            .shapes
            .types
            .source
            .func_members(&from)
            .collect::<Vec<_>>()
            .into_iter()
            .peekable();
        while let Some(identity) = members.next() {
            let case = func_tag(identity);
            let next = if members.peek().is_some() {
                let wanted = self.string(ctx, &case);
                let test = self.compare(ctx, CompareKind::Eq, tag, wanted);
                Some(self.enter_case(ctx, test))
            } else {
                None
            };
            let payload = self
                .shapes
                .callable_environment(identity)
                .map(|ty| self.callable_field(ctx, value, "val", ty))
                .transpose()?;
            let widened = self.function_value(ctx, identity, payload, Some(result))?;
            let widened = self.fit(ctx, widened, result)?;
            self.emit(BrOp::new(ctx, done, vec![widened]), ctx);
            if let Some(next) = next {
                self.switch_to(next);
            }
        }
        self.switch_to(done);
        Ok(done.deref(ctx).get_argument(0))
    }

    fn callable_operand(&mut self, ctx: &mut Context, operand: &'s Expr, ty: Ty) -> Result<Value> {
        let expected = self.shapes.callable_type(ty)?;
        let value = self.expression_with(ctx, operand, expected)?;
        match expected {
            Some(ty) => self.fit(ctx, value, ty),
            None => Ok(value),
        }
    }

    pub(super) fn function_value(
        &mut self,
        ctx: &mut Context,
        identity: FuncIdentity,
        payload: Option<Value>,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let tag = func_tag(identity);
        if let Some(target) = expected.map(|ty| present_type(ctx, ty))
            && let Some(variant) = perry_mir::typed::<VariantType>(ctx, target)
        {
            let expected_payload = variant
                .deref(ctx)
                .case(&tag)
                .ok_or_else(|| internal!("function {tag} is absent from its dispatch type"))?;
            let payload = match (payload, expected_payload) {
                (Some(value), Some(ty)) => Some(self.fit(ctx, value, ty)?),
                (None, None) => None,
                _ => bail_internal!("mismatched environment for function {tag}"),
            };
            return Ok(self.emit_value(VariantNewOp::new(ctx, target, &tag, payload), ctx));
        }
        let record = self
            .shapes
            .function_record(ctx, payload.map(|v| v.get_type(ctx)))?;
        let values = std::iter::once(self.string(ctx, &tag))
            .chain(payload)
            .collect();
        Ok(self.emit_value(AllocOp::new(ctx, record, values), ctx))
    }

    pub(super) fn matches_function(
        &mut self,
        ctx: &mut Context,
        arg: &'s Expr,
        source: Ty,
        identity: FuncIdentity,
        equal: bool,
    ) -> Result<Value> {
        let value = self.callable_operand(ctx, arg, source)?;
        let absent = self.boolean(ctx, !equal);
        let done = self.result_block(ctx, absent.get_type(ctx));
        for null in [true, false] {
            if let Some(test) = self.sentinel_test(ctx, value, null) {
                self.yield_if(ctx, done, test, [absent]);
            }
        }
        let value = self.defined(ctx, value)?;
        let tag = self.callable_field(ctx, value, "tag", StringType::get(ctx).into())?;
        let wanted = self.string(ctx, &func_tag(identity));
        let comparison = self.compare(
            ctx,
            if equal {
                CompareKind::Eq
            } else {
                CompareKind::Ne
            },
            tag,
            wanted,
        );
        Ok(self.finish_result(ctx, done, comparison))
    }

    pub(super) fn callable_field(
        &mut self,
        ctx: &mut Context,
        value: Value,
        name: &str,
        field_ty: TypeHandle,
    ) -> Result<Value> {
        let value = self.defined(ctx, value)?;
        if value.get_type(ctx).deref(ctx).is::<RecordType>() {
            let op = FieldGetOp::new(ctx, value, name.to_owned()).to_diag(ctx)?;
            Ok(self.emit_value(op, ctx))
        } else if name == "tag" {
            Ok(self.emit_value(VariantTagOp::new(ctx, value), ctx))
        } else {
            Ok(self.emit_value(VariantPayloadOp::new(ctx, value, field_ty), ctx))
        }
    }

    fn sentinel_test(&mut self, ctx: &mut Context, value: Value, null: bool) -> Option<Value> {
        let state = absence(ctx, value.get_type(ctx))?;
        if null {
            state
                .admits_null()
                .then(|| self.emit_value(IsNullOp::new(ctx, value), ctx))
        } else {
            state
                .admits_undefined()
                .then(|| self.emit_value(IsUndefinedOp::new(ctx, value), ctx))
        }
    }
}
