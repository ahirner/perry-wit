//! MIR representations and operations for finite sets of function identities.
use super::*;
use crate::engine::solve::CallTarget;
use crate::engine::types::{FuncIdentity, Ty, func_tag};
use perry_mir::{VariantNewOp, VariantPayloadOp, VariantTagOp, VariantType};

#[derive(Clone, Copy)]
pub(in crate::lower) enum Callback<'s> {
    Source(&'s Expr),
    Function(FuncId),
    Lexical(&'s Function<'s>, &'s Signature<'s>),
    Bound {
        call: CallTarget,
        params: &'s [Ty],
        callee: Value,
    },
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
        let plans = &self.shapes.specialization.instances;
        if let Some(crate::engine::solve::FuncCoercion::Callback { target, inputs }) =
            plans.site(self.local_owner, expr).coercion
        {
            return Ok(Callback::Bound {
                call: target,
                params: self.shapes.types.source.tuple_slice(inputs),
                callee: self.expression(ctx, expr)?,
            });
        }
        if let Some(id) = plans.reference(self.local_owner, expr) {
            if self
                .signatures
                .source(id)
                .and_then(|f| f.signature.as_ref()?.inline)
                == Some(crate::engine::solve::Invocation::Lexical)
            {
                let signature = self.signature(id)?;
                let function = signature
                    .inline
                    .ok_or_else(|| internal!("missing lexical callback body"))?;
                return Ok(Callback::Lexical(function, signature));
            }
            if let Some(expr) = self.shapes.specialization.lexical.closure(id) {
                return Ok(Callback::Source(expr));
            }
            return Ok(Callback::Function(id));
        }
        Ok(Callback::Source(expr))
    }

    pub(super) fn invoke_callback(
        &mut self,
        ctx: &mut Context,
        call: CallTarget,
        value: Option<Value>,
        receiver: Option<Value>,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let Some(target) = self
            .shapes
            .specialization
            .instances
            .emitted_callee(call)
            .transpose()?
        else {
            self.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
            return Ok(None);
        };
        let bound = self.bound_callee(ctx, call, value)?;
        let receiver = if call.needs_receiver() {
            Some(receiver.ok_or_else(|| internal!("missing callback receiver"))?)
        } else {
            None
        };
        self.call_values(
            ctx,
            target,
            bound
                .into_iter()
                .chain(receiver)
                .chain(args.iter().copied()),
        )
    }

    pub(super) fn bound_callee(
        &mut self,
        ctx: &mut Context,
        call: CallTarget,
        value: Option<Value>,
    ) -> Result<Option<Value>> {
        if !call.needs_value() {
            return Ok(None);
        }
        let value = value.ok_or_else(|| internal!("missing callable operand"))?;
        if matches!(call, CallTarget::Direct { pass_env: true, .. }) {
            let wrapper = self.defined(ctx, value)?;
            return self.record_field(ctx, wrapper, "val").map(Some);
        }
        Ok(Some(value))
    }

    pub(super) fn call_values(
        &mut self,
        ctx: &mut Context,
        target: FuncId,
        inputs: impl Iterator<Item = Value>,
    ) -> Result<Option<Value>> {
        let signature = self.signature(target)?;
        self.invoke_arguments(ctx, signature, inputs, Arguments::Plain(&[]), None)
    }

    pub(super) fn dispatch_body(
        &mut self,
        ctx: &mut Context,
        receiver: bool,
        arms: &[crate::engine::DispatchArm],
        args: &[Value],
    ) -> Result<()> {
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
                .chain(args.iter().copied().take(arm.arguments));
            let value = self.call_values(ctx, arm.target, inputs)?;
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
        let never = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
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
                self.yield_if(ctx, done, test, label);
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
                self.yield_if(ctx, done, test, right_absent.unwrap_or(no));
            }
            if let Some(test) = right_absent {
                self.yield_if(ctx, done, test, no);
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
            self.yield_if(ctx, done, differ, no);
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
                self.emit(ctx, |ctx| BrOp::new(ctx, done, vec![equal]));
                self.switch_to(next);
            }
        }
        Ok(self.finish_result(ctx, done, no))
    }

    pub(super) fn widen_function(
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
                self.yield_if(ctx, done, test, absent);
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
            let target = present_type(ctx, result);
            let widened = if let Some(record) = perry_mir::typed::<RecordType>(ctx, target) {
                let fields = std::iter::once(tag).chain(payload).collect();
                self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, fields))
            } else {
                self.emit_value(ctx, |ctx| VariantNewOp::new(ctx, target, &case, payload))
            };
            let widened = self.fit(ctx, widened, result)?;
            self.emit(ctx, |ctx| BrOp::new(ctx, done, vec![widened]));
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
                .cases()
                .iter()
                .find(|(name, _)| name == &tag)
                .map(|(_, ty)| *ty)
                .ok_or_else(|| internal!("function {tag} is absent from its dispatch type"))?;
            let payload = match (payload, expected_payload) {
                (Some(value), Some(ty)) => Some(self.fit(ctx, value, ty)?),
                (None, None) => None,
                _ => bail_internal!("mismatched environment for function {tag}"),
            };
            return Ok(self.emit_value(ctx, |ctx| VariantNewOp::new(ctx, target, &tag, payload)));
        }
        let mut fields = vec![("tag".into(), StringType::get(ctx).into())];
        let mut values = vec![self.string(ctx, &tag)];
        if let Some(value) = payload {
            fields.push(("val".into(), value.get_type(ctx)));
            values.push(value);
        }
        let record = self.shapes.mir_record(ctx, fields)?;
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)))
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
                self.yield_if(ctx, done, test, absent);
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
        self.emit(ctx, |ctx| BrOp::new(ctx, done, vec![comparison]));
        self.switch_to(done);
        Ok(done.deref(ctx).get_argument(0))
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
            Ok(self.append_value(ctx, &op))
        } else if name == "tag" {
            Ok(self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value)))
        } else {
            Ok(self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, field_ty)))
        }
    }

    fn sentinel_test(&mut self, ctx: &mut Context, value: Value, null: bool) -> Option<Value> {
        let state = absence(ctx, value.get_type(ctx))?;
        if null {
            state
                .admits_null()
                .then(|| self.emit_value(ctx, |ctx| IsNullOp::new(ctx, value)))
        } else {
            state
                .admits_undefined()
                .then(|| self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value)))
        }
    }

    fn result_block(&self, ctx: &mut Context, ty: TypeHandle) -> Ptr<BasicBlock> {
        let block = BasicBlock::new(ctx, None, vec![ty]);
        block.insert_at_back(self.region, ctx);
        block
    }

    fn enter_case(&mut self, ctx: &mut Context, test: Value) -> Ptr<BasicBlock> {
        let matched = self.new_block(ctx, "case");
        let next = self.new_block(ctx, "next");
        self.branch(ctx, test, matched, next);
        self.switch_to(matched);
        next
    }

    fn yield_if(&mut self, ctx: &mut Context, done: Ptr<BasicBlock>, test: Value, answer: Value) {
        let next = self.new_block(ctx, "next");
        self.emit(ctx, |ctx| {
            CondBrOp::new(ctx, test, done, vec![answer], next, vec![])
        });
        self.switch_to(next);
    }

    fn finish_result(&mut self, ctx: &mut Context, done: Ptr<BasicBlock>, answer: Value) -> Value {
        self.emit(ctx, |ctx| BrOp::new(ctx, done, vec![answer]));
        self.switch_to(done);
        done.deref(ctx).get_argument(0)
    }
}
