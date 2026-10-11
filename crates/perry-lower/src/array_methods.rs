use super::*;
use perry_mir::types::OptionalType;
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) const PUSH: &str = "$$perryArrayPush";

/// Keep arguments together before HIR expands a variadic push into separate mutations.
pub(super) fn preserve_arguments(module: &mut ast::Module) {
    struct Arguments;
    impl VisitMut for Arguments {
        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if (call.args.len() != 1 || call.args.iter().any(|argument| argument.spread.is_some()))
                && let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
                && method.sym == "push"
            {
                method.sym = PUSH.into();
            }
        }
    }
    module.visit_mut_with(&mut Arguments);
}

pub(super) struct CallbackReturn {
    done: Ptr<BasicBlock>,
    slot: Option<Value>,
    optional: bool,
    empty_returns: Vec<Ptr<BasicBlock>>,
}

fn returns_empty(statements: &[Stmt]) -> bool {
    let mut pending: Vec<_> = statements.iter().collect();
    while let Some(statement) = pending.pop() {
        match statement {
            Stmt::Return(None | Some(Expr::Undefined)) => return true,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                pending.extend(then_branch);
                pending.extend(else_branch.iter().flatten());
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::For { body, .. } => {
                pending.extend(body)
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                pending.extend(body);
                pending.extend(catch.iter().flat_map(|c| &c.body));
                pending.extend(finally.iter().flatten());
            }
            Stmt::Switch { cases, .. } => pending.extend(cases.iter().flat_map(|c| &c.body)),
            Stmt::Labeled { body, .. } => pending.push(body),
            _ => {}
        }
    }
    false
}

const FALLTHROUGH: u8 = 1;
const RETURN: u8 = 2;
const BREAK: u8 = 4;
const CONTINUE: u8 = 8;
const LABELLED: u8 = 16;

fn completions(statements: &[Stmt]) -> u8 {
    let mut result = FALLTHROUGH;
    for statement in statements {
        if result & FALLTHROUGH == 0 {
            break;
        }
        let next = match statement {
            Stmt::Return(_) | Stmt::Throw(_) => RETURN,
            Stmt::Break => BREAK,
            Stmt::Continue => CONTINUE,
            Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => LABELLED,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                completions(then_branch)
                    | else_branch
                        .as_ref()
                        .map_or(FALLTHROUGH, |body| completions(body))
            }
            Stmt::Switch { cases, .. } => {
                let mut tail = FALLTHROUGH;
                let mut branches = if cases.iter().any(|case| case.test.is_none()) {
                    0
                } else {
                    FALLTHROUGH
                };
                for case in cases.iter().rev() {
                    let body = completions(&case.body);
                    tail = (body & !FALLTHROUGH) | if body & FALLTHROUGH != 0 { tail } else { 0 };
                    branches |= tail;
                }
                (branches & !BREAK)
                    | if branches & BREAK != 0 {
                        FALLTHROUGH
                    } else {
                        0
                    }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let body = completions(body);
                (body & (RETURN | LABELLED))
                    | if matches!(condition, Expr::Bool(true)) && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::For {
                condition, body, ..
            } => {
                let body = completions(body);
                (body & (RETURN | LABELLED))
                    | if condition
                        .as_ref()
                        .is_none_or(|c| matches!(c, Expr::Bool(true)))
                        && body & BREAK == 0
                    {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body =
                    completions(body) | catch.as_ref().map_or(0, |catch| completions(&catch.body));
                let final_state = finally
                    .as_ref()
                    .map_or(FALLTHROUGH, |body| completions(body));
                (final_state & !FALLTHROUGH)
                    | if final_state & FALLTHROUGH != 0 {
                        body
                    } else {
                        0
                    }
            }
            _ => FALLTHROUGH,
        };
        result = (result & !FALLTHROUGH) | next;
    }
    result
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn callback_return(
        &mut self,
        ctx: &mut Context,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let callback = self.callback_returns.last().expect("callback");
        let (done, slot, optional) = (callback.done, callback.slot, callback.optional);
        if slot.is_none() && matches!(expression, None | Some(Expr::Undefined)) {
            self.callback_returns
                .last_mut()
                .expect("callback")
                .empty_returns
                .push(self.block);
            let finally = self.new_block(ctx, "callback_empty_return");
            self.jump(ctx, finally);
            self.switch_to(finally);
            self.run_finallies(ctx, |_| true)?;
            self.jump(ctx, done);
            self.after_terminator(ctx);
            return Ok(());
        }
        let expected = slot
            .map(|slot| self.load(ctx, slot).map(|v| v.get_type(ctx)))
            .transpose()?;
        let value = match expression {
            Some(expression) => self.expression_with(ctx, expression, expected)?,
            None => match expected {
                Some(ty) => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
                None => {
                    let ty = FP64Type::get(ctx).into();
                    self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
                }
            },
        };
        let slot = match slot {
            Some(slot) => slot,
            None => {
                let ty = if optional {
                    OptionalType::get(ctx, value.get_type(ctx)).into()
                } else {
                    value.get_type(ctx)
                };
                let slot = self.entry_slot(ctx, ty);
                self.callback_returns.last_mut().expect("callback").slot = Some(slot);
                slot
            }
        };
        let ty = self.slot_type(ctx, slot)?;
        let value = self.fit(ctx, value, ty)?;
        self.store(ctx, slot, value);
        self.run_finallies(ctx, |_| true)?;
        self.jump(ctx, done);
        self.after_terminator(ctx);
        Ok(())
    }

    fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
    ) -> Result<Value> {
        if let Expr::FuncRef(id) = callback {
            let signature = self.signatures.get(id).context("unknown callback")?;
            ensure!(
                !signature.is_async
                    && signature.params.len() <= arguments.len()
                    && signature.results.len() <= 1,
                "array callbacks require synchronous functions with at most three parameters"
            );
            let values = arguments
                .iter()
                .zip(&signature.params)
                .map(|(&v, &ty)| self.fit(ctx, v, ty))
                .collect::<Result<Vec<_>>>()?;
            let call = CallOp::new(
                ctx,
                signature.name.clone(),
                values,
                signature.results.clone(),
            );
            self.append(ctx, &call);
            return Ok(if signature.results.is_empty() {
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
            } else {
                call.get_operation().deref(ctx).get_result(0)
            });
        }
        let Expr::Closure {
            params,
            body,
            is_async,
            is_generator,
            return_type,
            ..
        } = callback
        else {
            bail!("array callbacks require an inline closure or named function")
        };
        ensure!(
            !is_async
                && !is_generator
                && params.len() <= arguments.len()
                && params.iter().all(|p| !p.is_rest && p.default.is_none()),
            "array callbacks must be synchronous with at most three parameters; default and rest parameters are not supported"
        );
        let saved_locals = self.locals.clone();
        let saved_guards = self.variant_guards.clone();
        for (param, &value) in params.iter().zip(arguments) {
            let slot = self.entry_slot(ctx, value.get_type(ctx));
            self.store(ctx, slot, value);
            self.locals.insert(param.id, slot);
        }
        let finallies = std::mem::take(&mut self.finallies);
        let controls = std::mem::take(&mut self.controls);
        let done = self.new_block(ctx, "callback_done");
        let optional = completions(body) & FALLTHROUGH != 0 || returns_empty(body);
        let declared = match return_type {
            HirType::Number | HirType::String | HirType::Boolean | HirType::Union(_) => {
                self.shapes.value_type(ctx, return_type).ok()
            }
            _ => None,
        };
        let slot = declared.map(|ty| {
            let ty = if optional {
                OptionalType::get(ctx, ty).into()
            } else {
                ty
            };
            self.entry_slot(ctx, ty)
        });
        self.callback_returns.push(CallbackReturn {
            done,
            slot,
            optional,
            empty_returns: Vec::new(),
        });
        let result = self.statements(ctx, body);
        if result.is_ok() && self.reachable(ctx, self.block) {
            self.callback_return(ctx, None)?;
        }
        let callback = self.callback_returns.pop().expect("callback");
        let slot = callback.slot.or_else(|| {
            if callback.empty_returns.is_empty() {
                return None;
            }
            let number = FP64Type::get(ctx).into();
            let ty = OptionalType::get(ctx, number).into();
            Some(self.entry_slot(ctx, ty))
        });
        for block in callback.empty_returns {
            let slot = slot.expect("empty return slot");
            let ty = self.slot_type(ctx, slot)?;
            let undefined = UndefinedOp::with_type(ctx, ty);
            let store = StoreOp::new(ctx, slot, undefined.get_result(ctx));
            let terminator = block.deref(ctx).get_terminator(ctx).expect("return branch");
            undefined.get_operation().insert_before(ctx, terminator);
            store.get_operation().insert_before(ctx, terminator);
        }
        self.locals = saved_locals;
        self.variant_guards = saved_guards;
        self.finallies = finallies;
        self.controls = controls;
        result?;
        self.switch_to(done);
        match slot {
            Some(slot) => self.load(ctx, slot),
            None => {
                let ty = FP64Type::get(ctx).into();
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty)))
            }
        }
    }

    pub(super) fn array_transform(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        filter: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
        let limit = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        });
        let zero = self.number(ctx, 0.0);
        let index = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, index, zero);
        let prepare = self.block;
        let header = self.new_block(ctx, "array_header");
        let body = self.new_block(ctx, "array_body");
        let done = self.new_block(ctx, "array_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, body, done);
        self.switch_to(body);
        let next = self.new_block(ctx, "array_next");
        let available = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
        let available = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, available)
        });
        let present = self.compare(ctx, CompareKind::Lt, current, available);
        let read = self.new_block(ctx, "array_read");
        self.branch(ctx, present, read, next);
        self.switch_to(read);
        let at = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::F64ToU32, current)
        });
        let op = perry_mir::collections::ArrayAtOp::new(ctx, array, at)?;
        let item = self.append_value(ctx, &op);
        let mapped = self.array_callback(ctx, callback, &[item, current, array])?;
        let append = self.block;
        let element = if filter {
            item.get_type(ctx)
        } else {
            mapped.get_type(ctx)
        };
        let allocation = ArrayNewOp::new(ctx, element, vec![]);
        let terminator = prepare
            .deref(ctx)
            .get_terminator(ctx)
            .expect("array entry branch");
        allocation.get_operation().insert_before(ctx, terminator);
        let result = allocation.get_result(ctx);
        self.switch_to(append);
        if filter {
            let keep = self.truthy(ctx, mapped)?;
            let push = self.new_block(ctx, "array_keep");
            self.branch(ctx, keep, push, next);
            self.switch_to(push);
        }
        self.emit(ctx, |ctx| {
            ArrayPushOp::new(ctx, result, if filter { item } else { mapped })
        });
        self.jump(ctx, next);
        self.switch_to(next);
        let one = self.number(ctx, 1.0);
        let advanced = self.binary(ctx, BinaryKind::Add, current, one);
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        Ok(result)
    }

    pub(super) fn array_search(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: &'s Expr,
        from: Option<&'s Expr>,
        includes: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let needle = self.expression_with(ctx, value, Some(element))?;
        let needle = self.fit(ctx, needle, element)?;
        let number = FP64Type::get(ctx).into();
        let zero = self.number(ctx, 0.0);
        let start = match from {
            Some(from) => self.number_operand(ctx, from)?,
            None => zero,
        };
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
        let limit = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        });
        let start = self.intrinsic(ctx, Intrinsic::MathTrunc, vec![start]);
        let valid = self.compare(ctx, CompareKind::Eq, start, start);
        let start = self.choose(ctx, number, valid, |_, _| Ok(start), |_, _| Ok(zero))?;
        let negative = self.compare(ctx, CompareKind::Lt, start, zero);
        let start = self.choose(
            ctx,
            number,
            negative,
            |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, limit, start)),
            |_, _| Ok(start),
        )?;
        let below = self.compare(ctx, CompareKind::Lt, start, zero);
        let start = self.choose(ctx, number, below, |_, _| Ok(zero), |_, _| Ok(start))?;
        let index = self.entry_slot(ctx, number);
        self.store(ctx, index, start);
        let miss = if includes {
            self.boolean(ctx, false)
        } else {
            self.number(ctx, -1.0)
        };
        let result = self.entry_slot(ctx, miss.get_type(ctx));
        self.store(ctx, result, miss);
        let header = self.new_block(ctx, "search_header");
        let body = self.new_block(ctx, "search_body");
        let found = self.new_block(ctx, "search_found");
        let next = self.new_block(ctx, "search_next");
        let done = self.new_block(ctx, "search_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, body, done);
        self.switch_to(body);
        let at = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::F64ToU32, current)
        });
        let op = perry_mir::collections::ArrayAtOp::new(ctx, array, at)?;
        let item = self.append_value(ctx, &op);
        let equal = self.compare(ctx, CompareKind::Eq, item, needle);
        if includes {
            let nan = self.new_block(ctx, "search_nan");
            self.branch(ctx, equal, found, nan);
            self.switch_to(nan);
            let item_nan = self.compare(ctx, CompareKind::Ne, item, item);
            let needle_nan = self.compare(ctx, CompareKind::Ne, needle, needle);
            let both = self.choose(
                ctx,
                item_nan.get_type(ctx),
                item_nan,
                |_, _| Ok(needle_nan),
                |_, _| Ok(item_nan),
            )?;
            self.branch(ctx, both, found, next);
        } else {
            self.branch(ctx, equal, found, next);
        }
        self.switch_to(found);
        let value = if includes {
            self.boolean(ctx, true)
        } else {
            current
        };
        self.store(ctx, result, value);
        self.jump(ctx, done);
        self.switch_to(next);
        let one = self.number(ctx, 1.0);
        let advanced = self.binary(ctx, BinaryKind::Add, current, one);
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        self.load(ctx, result)
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn array_push_spread(
        &mut self,
        ctx: &mut Context,
        callee: &'s Expr,
        args: &'s [perry_hir::CallArg],
    ) -> Result<Value> {
        let Expr::PropertyGet {
            object, property, ..
        } = callee
        else {
            bail!("spread calls require a dense array push receiver")
        };
        ensure!(
            property == PUSH || property == "push",
            "spread calls are supported for dense array push"
        );
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        ensure!(
            kind(ctx, receiver)? == Kind::Array,
            "push requires a dense array receiver"
        );
        let element = self.array_element(ctx, receiver)?;
        let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expression) => {
                    let value = self.expression_with(ctx, expression, Some(element))?;
                    let value = self.fit(ctx, value, element)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, values, value));
                }
                perry_hir::CallArg::Spread(expression) => {
                    let source = self.expression(ctx, expression)?;
                    let source = self.defined(ctx, source)?;
                    ensure!(
                        self.array_element(ctx, source)? == element,
                        "spread elements must fit the destination array"
                    );
                    self.append_array(ctx, values, source)?;
                }
            }
        }
        self.append_array(ctx, receiver, values)?;
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, receiver));
        Ok(self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        }))
    }

    fn append_array(&mut self, ctx: &mut Context, target: Value, source: Value) -> Result<()> {
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, source));
        let limit = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        });
        let zero = self.number(ctx, 0.0);
        let index = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, index, zero);
        let header = self.new_block(ctx, "spread_header");
        let body = self.new_block(ctx, "spread_body");
        let done = self.new_block(ctx, "spread_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, body, done);
        self.switch_to(body);
        let at = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::F64ToU32, current)
        });
        let get = perry_mir::collections::ArrayAtOp::new(ctx, source, at)?;
        let value = self.append_value(ctx, &get);
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, target, value));
        let one = self.number(ctx, 1.0);
        let advanced = self.binary(ctx, BinaryKind::Add, current, one);
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        Ok(())
    }
}
