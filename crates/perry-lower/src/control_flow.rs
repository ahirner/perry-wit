//! Structured source control flow expressed as ordinary typed MIR blocks.
use super::*;

impl FunctionBuilder<'_> {
    pub(super) fn reachable(&self, ctx: &Context, block: Ptr<BasicBlock>) -> bool {
        let entry = self.region.deref(ctx).iter(ctx).next().expect("entry");
        let mut seen = HashSet::new();
        let mut pending = vec![block];
        while let Some(block) = pending.pop() {
            if block == entry {
                return true;
            }
            if seen.insert(block) {
                pending.extend(block.preds(ctx));
            }
        }
        false
    }

    pub(super) fn switch(
        &mut self,
        ctx: &mut Context,
        discriminant: &Expr,
        cases: &[perry_hir::ir::SwitchCase],
    ) -> Result<()> {
        let value = self.expression(ctx, discriminant)?;
        let ty = value.get_type(ctx);
        ensure!(
            matches!(
                kind(ctx, value)?,
                Kind::Number | Kind::String | Kind::Boolean | Kind::BigInt
            ),
            "switch requires a typed scalar discriminant"
        );
        let done = self.new_block(ctx, "switch_done");
        let blocks: Vec<_> = cases
            .iter()
            .map(|_| self.new_block(ctx, "switch_case"))
            .collect();
        let default = cases
            .iter()
            .position(|case| case.test.is_none())
            .map_or(done, |index| blocks[index]);
        ensure!(
            cases.iter().filter(|case| case.test.is_none()).count() <= 1,
            "switch has more than one default"
        );
        for (case, &block) in cases.iter().zip(&blocks) {
            if let Some(test) = &case.test {
                let test = self.expression_with(ctx, test, Some(ty))?;
                let test = self.fit(ctx, test, ty)?;
                let matches = self.compare(ctx, CompareKind::Eq, value, test);
                let next = self.new_block(ctx, "switch_test");
                self.branch(ctx, matches, block, next);
                self.switch_to(next);
            }
        }
        self.jump(ctx, default);
        self.controls.push(Control::Switch { break_to: done });
        let guards = self.variant_guards.len();
        let mut fallthrough = false;
        for (index, case) in cases.iter().enumerate() {
            self.variant_guards.truncate(guards);
            if !fallthrough {
                if let Some(test) = &case.test {
                    self.variant_equality(discriminant, test, true);
                } else {
                    for test in cases.iter().filter_map(|case| case.test.as_ref()) {
                        self.variant_equality(discriminant, test, false);
                    }
                }
            }
            self.switch_to(blocks[index]);
            self.statements(ctx, &case.body)?;
            fallthrough = self.reachable(ctx, self.block);
            self.jump(ctx, blocks.get(index + 1).copied().unwrap_or(done));
        }
        self.variant_guards.truncate(guards);
        self.controls.pop();
        self.switch_to(done);
        Ok(())
    }
}
