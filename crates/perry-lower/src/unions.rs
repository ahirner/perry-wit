//! Local union refinements are valid only along paths that preserve the guarded binding.
use super::*;

pub(super) fn written_locals<'a>(
    statements: impl IntoIterator<Item = &'a Stmt>,
    expressions: &[&Expr],
) -> HashSet<LocalId> {
    fn expression(value: &Expr, writes: &mut HashSet<LocalId>) {
        if let Expr::LocalSet(id, _) | Expr::Update { id, .. } = value {
            writes.insert(*id);
        }
        perry_hir::walker::walk_expr_children(value, &mut |child| expression(child, writes));
    }
    fn statement(value: &Stmt, writes: &mut HashSet<LocalId>) {
        match value {
            Stmt::Expr(value) | Stmt::Throw(value) | Stmt::Return(Some(value)) => {
                expression(value, writes)
            }
            Stmt::Let { id, init, .. } => {
                writes.insert(*id);
                if let Some(value) = init {
                    expression(value, writes);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                expression(condition, writes);
                for value in then_branch.iter().chain(else_branch.iter().flatten()) {
                    statement(value, writes);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                expression(condition, writes);
                for value in body {
                    statement(value, writes);
                }
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    statement(init, writes);
                }
                for value in condition.iter().chain(update.iter()) {
                    expression(value, writes);
                }
                for value in body {
                    statement(value, writes);
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                for value in body
                    .iter()
                    .chain(catch.iter().flat_map(|catch| &catch.body))
                    .chain(finally.iter().flatten())
                {
                    statement(value, writes);
                }
            }
            _ => {}
        }
    }
    let mut writes = HashSet::new();
    for value in statements {
        statement(value, &mut writes);
    }
    for value in expressions {
        expression(value, &mut writes);
    }
    writes
}

impl FunctionBuilder<'_> {
    pub(super) fn refine_condition(
        &mut self,
        ctx: &mut Context,
        condition: &Expr,
        truth: bool,
    ) -> Result<()> {
        fn guards(condition: &Expr, truth: bool, out: &mut Vec<(LocalId, bool, bool)>) {
            match condition {
                Expr::Unary {
                    op: UnaryOp::Not,
                    operand,
                } => guards(operand, !truth, out),
                Expr::Logical { op, left, right }
                    if matches!((op, truth), (LogicalOp::And, true) | (LogicalOp::Or, false)) =>
                {
                    guards(left, truth, out);
                    guards(right, truth, out);
                }
                Expr::Compare {
                    op: CompareOp::Eq | CompareOp::Ne,
                    left,
                    right,
                } => {
                    let (value, tag) = match (&**left, &**right) {
                        (Expr::TypeOf(value), Expr::String(tag))
                        | (Expr::String(tag), Expr::TypeOf(value)) => (value, tag),
                        _ => return,
                    };
                    if let Expr::LocalGet(id) = **value
                        && matches!(tag.as_str(), "string" | "object")
                    {
                        let equal = matches!(
                            condition,
                            Expr::Compare {
                                op: CompareOp::Eq,
                                ..
                            }
                        );
                        out.push((id, (tag == "string") == (truth == equal), truth == equal));
                    }
                }
                _ => {}
            }
        }
        let mut facts = Vec::new();
        guards(condition, truth, &mut facts);
        let writes = written_locals(&[], &[condition]);
        for (id, text, present) in facts {
            if writes.contains(&id) {
                continue;
            }
            let slot = self.local(id)?;
            let source = self.slot_type(ctx, slot)?;
            let optional = optional_value(ctx, source);
            if optional
                .unwrap_or(source)
                .deref(ctx)
                .is::<TextOrBytesType>()
            {
                let ty = if text {
                    StringType::get(ctx).into()
                } else {
                    BytesType::get(ctx).into()
                };
                let ty = if optional.is_some() && !present {
                    perry_mir::types::OptionalType::get(ctx, ty).into()
                } else {
                    ty
                };
                self.refinements.insert(id, ty);
            }
        }
        Ok(())
    }
    pub(super) fn text_bytes_equal(
        &mut self,
        ctx: &mut Context,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value> {
        let mut values = Vec::with_capacity(2);
        for value in [lhs, rhs] {
            let ty = optional_value(ctx, value.get_type(ctx)).unwrap_or(value.get_type(ctx));
            let value = if ty.deref(ctx).is::<TextOrBytesType>() {
                value
            } else if ty.deref(ctx).is::<StringType>() || ty.deref(ctx).is::<BytesType>() {
                self.emit_value(ctx, |ctx| perry_mir::unions::InjectOp::new(ctx, value))
            } else {
                if self.is_optional(ctx, lhs) && self.is_optional(ctx, rhs) {
                    let missing = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, lhs));
                    let no = self.boolean(ctx, false);
                    let slot = self.entry_slot(ctx, no.get_type(ctx));
                    self.store(ctx, slot, no);
                    let check = self.new_block(ctx, "union_equal_undefined");
                    let done = self.new_block(ctx, "union_equal_done");
                    self.branch(ctx, missing, check, done);
                    self.switch_to(check);
                    let missing = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, rhs));
                    self.store(ctx, slot, missing);
                    self.jump(ctx, done);
                    self.switch_to(done);
                    return self.load(ctx, slot);
                }
                return Ok(self.boolean(ctx, false));
            };
            values.push(self.optional(ctx, value));
        }
        Ok(self
            .intrinsic(ctx, Intrinsic::TextBytesEqual, values)
            .unwrap())
    }
}

pub(super) fn merge_refinements(
    mut left: HashMap<LocalId, TypeHandle>,
    right: &HashMap<LocalId, TypeHandle>,
) -> HashMap<LocalId, TypeHandle> {
    left.retain(|id, ty| right.get(id) == Some(ty));
    left
}
