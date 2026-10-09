//! Native aggregate fast paths and checked access through JSON-backed typed views.
use super::*;
use pliron_wasm::control::UnreachableOp;

impl FunctionLowering<'_> {
    fn aggregate_branch(
        &mut self,
        ctx: &mut Context,
        reference: Value,
        result: Option<TypeHandle>,
        native: impl FnOnce(&mut Self, &mut Context) -> Result<Option<Value>>,
        view: impl FnOnce(&mut Self, &mut Context) -> Result<Option<Value>>,
    ) -> Result<Option<Value>> {
        if !self.state.uses_typed_views {
            return native(self, ctx);
        }
        let tag = self.i32_const(ctx, 1);
        let tagged = self.num(ctx, NumOpcode::I32And, vec![reference, tag]);
        let direct = self.new_block(ctx, vec![]);
        let checked = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, result.into_iter().collect());
        self.cond_br(ctx, tagged, (checked, vec![]), (direct, vec![]));
        self.block = direct;
        let value = native(self, ctx)?;
        self.br(ctx, done, value.into_iter().collect());
        self.block = checked;
        let value = view(self, ctx)?;
        self.br(ctx, done, value.into_iter().collect());
        self.block = done;
        Ok(result.map(|_| done.deref(ctx).get_argument(0)))
    }

    pub(super) fn decode_slot_bits(
        &mut self,
        ctx: &mut Context,
        bits: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let opcode = if repr(ctx, ty)? == Repr::F64 {
            NumOpcode::F64ReinterpretI64
        } else {
            NumOpcode::I32WrapI64
        };
        Ok(self.num(ctx, opcode, vec![bits]))
    }

    pub(super) fn check_pending(&mut self, ctx: &mut Context) {
        let status = self
            .runtime(ctx, Runtime::ExceptionStatus, vec![])
            .expect("status");
        if self.throws {
            self.throw_status_if(ctx, status, status);
        } else {
            // Canonical result lowering has no guest catch scope or WIT error result.
            let failed = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            self.cond_br(ctx, status, (failed, vec![]), (next, vec![]));
            self.block = failed;
            self.abort_task(ctx);
            let trap = UnreachableOp::new(ctx).get_operation();
            self.insert(ctx, trap);
            self.block = next;
        }
    }

    fn view_get(
        &mut self,
        ctx: &mut Context,
        reference: Value,
        index: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let bits = self
            .runtime(ctx, Runtime::ViewGet, vec![reference, index])
            .expect("slot");
        self.check_pending(ctx);
        self.decode_slot_bits(ctx, bits, ty)
    }

    fn view_set(&mut self, ctx: &mut Context, reference: Value, index: Value, value: Value) {
        let bits = self.slot_bits(ctx, value);
        self.runtime(ctx, Runtime::ViewSet, vec![reference, index, bits]);
        self.check_pending(ctx);
    }

    pub(super) fn read_field(
        &mut self,
        ctx: &mut Context,
        reference: Value,
        offset: u32,
        ty: TypeHandle,
    ) -> Result<Value> {
        let result = wasm_type(ctx, ty)?;
        let kind = slot_kind(ctx, ty)?;
        self.aggregate_branch(
            ctx,
            reference,
            Some(result),
            |b, ctx| Ok(Some(b.load(ctx, kind, reference, offset))),
            |b, ctx| {
                let index = b.i32_const(ctx, ((offset - 8) / 8) as i32);
                Ok(Some(b.view_get(ctx, reference, index, ty)?))
            },
        )
        .map(Option::unwrap)
    }

    pub(super) fn write_field(
        &mut self,
        ctx: &mut Context,
        reference: Value,
        offset: u32,
        value: Value,
        kind: MemKind,
        optional: bool,
    ) -> Result<()> {
        self.aggregate_branch(
            ctx,
            reference,
            None,
            |b, ctx| {
                b.store(ctx, kind, reference, offset, value);
                if optional {
                    let index = b.i32_const(ctx, ((offset - 8) / 8) as i32);
                    b.runtime(ctx, Runtime::RecordInclude, vec![reference, index]);
                }
                Ok(None)
            },
            |b, ctx| {
                let index = b.i32_const(ctx, ((offset - 8) / 8) as i32);
                b.view_set(ctx, reference, index, value);
                Ok(None)
            },
        )?;
        Ok(())
    }

    pub(super) fn array_length(&mut self, ctx: &mut Context, array: Value) -> Result<Value> {
        let result = i32_type(ctx);
        self.aggregate_branch(
            ctx,
            array,
            Some(result),
            |b, ctx| Ok(Some(b.load(ctx, MemKind::I32, array, layout::LEN))),
            |b, ctx| Ok(b.runtime(ctx, Runtime::ViewLength, vec![array])),
        )
        .map(Option::unwrap)
    }

    /// Read an in-range element.
    pub(super) fn read_element(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let result = wasm_type(ctx, ty)?;
        let kind = slot_kind(ctx, ty)?;
        self.aggregate_branch(
            ctx,
            array,
            Some(result),
            |b, ctx| {
                let data = b.load(ctx, MemKind::I32, array, layout::DATA);
                let address = b.slot_address(ctx, data, index);
                Ok(Some(b.load(ctx, kind, address, 0)))
            },
            |b, ctx| Ok(Some(b.view_get(ctx, array, index, ty)?)),
        )
        .map(Option::unwrap)
    }

    pub(super) fn write_element(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
        value: Value,
        kind: MemKind,
    ) -> Result<()> {
        self.aggregate_branch(
            ctx,
            array,
            None,
            |b, ctx| {
                let data = b.load(ctx, MemKind::I32, array, layout::DATA);
                let address = b.slot_address(ctx, data, index);
                b.store(ctx, kind, address, 0, value);
                Ok(None)
            },
            |b, ctx| {
                b.view_set(ctx, array, index, value);
                Ok(None)
            },
        )?;
        Ok(())
    }

    pub(super) fn push(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: Value,
        kind: MemKind,
    ) -> Result<()> {
        self.aggregate_branch(
            ctx,
            array,
            None,
            |b, ctx| {
                b.push_native(ctx, array, value, kind);
                Ok(None)
            },
            |b, ctx| {
                let len = b
                    .runtime(ctx, Runtime::ViewLength, vec![array])
                    .expect("length");
                b.view_set(ctx, array, len, value);
                Ok(None)
            },
        )?;
        Ok(())
    }
}
