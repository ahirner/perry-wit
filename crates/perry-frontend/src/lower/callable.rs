//! MIR representations and operations for finite sets of function identities.
use super::*;
use crate::engine::{CallbackOperand, NativeCallback};
use crate::engine::{
    solve::CallableOp,
    types::{FuncIdentity, Idx, Ty, TyKind, env_class_name, func_tag},
};
use perry_mir::{VariantNewOp, VariantPayloadOp, VariantTagOp, VariantType};

#[derive(Clone, Copy)]
pub(in crate::lower) enum Callback<'s> {
    Source(&'s Expr),
    Bound(&'s crate::engine::CallbackBinding, Value),
}

impl<'s> Shapes<'s> {
    pub(super) fn bound_callback(
        &self,
        expr: &'s Expr,
    ) -> Option<(&'s crate::engine::CallbackBinding, &'s Expr)> {
        let Expr::Call { callee, args, .. } = expr else {
            return None;
        };
        let (Expr::FuncRef(id), [value]) = (&**callee, args.as_slice()) else {
            return None;
        };
        let crate::engine::NativeOperation::Callback(plan) =
            self.specialization.instances.operations.get(id)?
        else {
            return None;
        };
        Some((plan, value))
    }

    fn environment_type(
        &self,
        ctx: &Context,
        function: FuncIdentity,
    ) -> Result<Option<TypeHandle>> {
        self.specialization
            .escaping_envs
            .contains(function.template.index())
            .then(|| self.named(ctx, &env_class_name(function)))
            .transpose()
    }

    pub(super) fn callable_type(&self, ctx: &Context, ty: Ty) -> Result<Option<TypeHandle>> {
        let types = self.specialization.types.borrow();
        let mut identities = Vec::new();
        let mut absent = None;
        let members = match types.kind(ty) {
            TyKind::Union(span) => types.ty_slice(span),
            _ => std::slice::from_ref(&ty),
        };
        for &member in members {
            let state = match types.kind(member) {
                TyKind::Func(identity) => {
                    identities.push(identity);
                    continue;
                }
                TyKind::Void => perry_mir::Absence::Undefined,
                TyKind::Null => perry_mir::Absence::Null,
                _ => return Ok(None),
            };
            absent = perry_mir::Absence::join_optional(absent, Some(state));
        }
        drop(types);
        let cases = identities
            .into_iter()
            .map(|identity| Ok((func_tag(identity), self.environment_type(ctx, identity)?)))
            .collect::<Result<Vec<_>>>()?;
        let present = match cases.as_slice() {
            [] => return Ok(None),
            [(_, payload)] => {
                let mut fields = vec![("tag".into(), StringType::get(ctx).into())];
                if let Some(payload) = payload {
                    fields.push(("val".into(), *payload));
                }
                self.mir_record(ctx, fields)?.into()
            }
            _ => VariantType::get(ctx, cases).into(),
        };
        Ok(Some(absent.map_or(present, |state| {
            perry_mir::OptionalType::get_with(ctx, present, state).into()
        })))
    }
}

impl crate::engine::NativeCallback {
    pub(super) fn take_operand(self, args: &mut &[Value]) -> Result<Option<Value>> {
        if !self.takes_value() {
            return Ok(None);
        }
        let (&value, rest) = args
            .split_first()
            .ok_or_else(|| internal!("missing callback argument"))?;
        *args = rest;
        Ok(Some(value))
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(in crate::lower) fn prepare_callback(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<Callback<'s>> {
        let expr = self.shapes.callable(expr);
        Ok(match self.shapes.bound_callback(expr) {
            Some((plan, expr)) => Callback::Bound(plan, self.expression(ctx, expr)?),
            None => Callback::Source(expr),
        })
    }

    pub(super) fn invoke_callback(
        &mut self,
        ctx: &mut Context,
        call: NativeCallback,
        value: Option<Value>,
        receiver: Option<Value>,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let NativeCallback::Call {
            target,
            operand,
            receiver: needs_receiver,
        } = call
        else {
            self.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
            return Ok(None);
        };
        let bound = match operand {
            CallbackOperand::None => None,
            CallbackOperand::Callee => {
                Some(value.ok_or_else(|| internal!("missing callback value"))?)
            }
            CallbackOperand::Environment => {
                let wrapper = value.ok_or_else(|| internal!("missing callback environment"))?;
                let wrapper = self.defined(ctx, wrapper)?;
                Some(self.record_field(ctx, wrapper, "val")?)
            }
        };
        let receiver = if needs_receiver {
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

    pub(super) fn call_values(
        &mut self,
        ctx: &mut Context,
        target: FuncId,
        mut inputs: impl Iterator<Item = Value>,
    ) -> Result<Option<Value>> {
        let signature = self.signature(target)?;
        let (fixed, rest) = signature.parameters();
        let mut values = Vec::with_capacity(signature.params.len());
        for parameter in fixed {
            values.push(match inputs.next() {
                Some(value) => self.fixed_value(ctx, parameter, value)?,
                None => self.bind_fixed_arg(ctx, parameter, None)?,
            });
        }
        if let Some(rest) = rest {
            let element = Self::rest_element_type(ctx, rest.binding.carrier)?;
            let items = inputs
                .map(|v| self.fit(ctx, v, element))
                .collect::<Result<Vec<_>>>()?;
            values.push(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, items)));
        }
        self.emit_signature_call(ctx, signature, values)
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
                .environment_type(ctx, arm.identity)?
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

    pub(super) fn callable_operation(
        &mut self,
        ctx: &mut Context,
        operation: CallableOp,
        args: &'s [Expr],
    ) -> Result<Value> {
        match (operation, args) {
            (CallableOp::ThrowTypeError, []) => {
                self.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
                let ty = void_type(ctx);
                let never = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
                self.never_values.insert(never);
                Ok(never)
            }
            (CallableOp::TypeOfFn(ty), [operand]) => {
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
            (CallableOp::Widener { from, to }, [operand]) => {
                let value = self.callable_operand(ctx, operand, from)?;
                let result = self
                    .shapes
                    .callable_type(ctx, to)?
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
                    .specialization
                    .types
                    .borrow()
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
                        .environment_type(ctx, identity)?
                        .map(|ty| self.callable_field(ctx, value, "val", ty))
                        .transpose()?;
                    let target = present_type(ctx, result);
                    let widened = if let Some(record) = perry_mir::typed::<RecordType>(ctx, target)
                    {
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
            (CallableOp::Comparer { left_ty, right_ty }, [a, b]) => {
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
                let types = self.shapes.specialization.types.borrow();
                let shared = types
                    .func_members(&left_ty)
                    .filter(|id| types.func_members(&right_ty).any(|other| other == *id))
                    .collect::<Vec<_>>();
                drop(types);
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
                        let equal =
                            if let Some(env) = self.shapes.environment_type(ctx, identity)? {
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
            _ => bail_internal!("invalid operands for solved function operation"),
        }
    }

    fn callable_operand(&mut self, ctx: &mut Context, operand: &'s Expr, ty: Ty) -> Result<Value> {
        let expected = self.shapes.callable_type(ctx, ty)?;
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
