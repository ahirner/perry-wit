//! Canonical lowering of statically bound host calls.
use super::*;
use perry_abi::layout::foreign as transfer;
use pliron::{context::Ptr, operation::Operation, r#type::Typed, utils::vec_exns::VecExtns};

impl FunctionLowering<'_, '_> {
    pub(crate) fn foreign_call(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        binding: &str,
    ) -> Result<()> {
        let import = self.state.bindings.get(binding).copied().ok_or_else(|| {
            diag!(
                codes::CONTRACT_MISMATCH,
                "WIT import {binding} is absent from the selected world"
            )
        })?;
        let signature = match &import.kind {
            ImportKind::Function(signature) => signature,
            ImportKind::Resource { key, operation } => {
                return self.resource_helper(ctx, op, *key, *operation);
            }
        };
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        ensure_diag!(
            operands.len() == import.params.len(),
            codes::CONTRACT_MISMATCH,
            "WIT import argument count mismatch"
        );
        let values = operands
            .iter()
            .map(|mir| self.mapped_value(ctx, *mir))
            .collect::<Result<Vec<_>>>()?;
        let mark = self.root_mark(ctx);
        for (mir, value) in operands.iter().zip(&values) {
            if repr(ctx, mir.get_type(ctx))? == Repr::Ref {
                self.publish_roots(ctx, &[value.scalar()?]);
            }
        }
        self.begin_resources(
            ctx,
            import.params.iter().any(AbiType::has_resources),
            Some(mark),
        )?;
        self.canonical_borrows = true;
        let mut args = Vec::new();
        let param_area = import.indirect_size.map(|size| {
            let bytes = self.i32_const(ctx, size as i32);
            self.alloc(ctx, bytes)
        });
        for ((abi, mir), value) in import.params.iter().zip(&operands).zip(&values) {
            let target = match param_area {
                Some(area) => Canonical::Memory(area, abi.offset),
                None => Canonical::Flat(&mut args),
            };
            self.lower_abi(
                ctx,
                abi,
                BoundaryValue::Native(mir.get_type(ctx), *value),
                target,
            )?;
        }
        if let Some(area) = param_area {
            args.push(area);
        }
        let area = if let Some(result) = &import.result
            && signature.retptr
        {
            let size = self.i32_const(ctx, result.size as i32);
            let area = self.alloc(ctx, size);
            args.push(area);
            Some(area)
        } else {
            None
        };
        let transfer = if import.asynchronous {
            let owners = self.runtime(ctx, Runtime::RootsCapture, vec![mark]);
            let size = self.i32_const(ctx, transfer::SIZE as i32);
            let shape = self
                .state
                .data
                .pointer_shape(&[transfer::AREA, transfer::OWNERS]);
            let shape = self.i32_const(ctx, shape as i32);
            let transfer = self.runtime(ctx, Runtime::GcAlloc, vec![size, shape]);
            let index = self
                .state
                .foreign_transfers
                .iter()
                .position(|import| import.binding == binding)
                .unwrap_or_else(|| self.state.foreign_transfers.push_back(import));
            let kind = self.i32_const(ctx, index as i32);
            let area = area.unwrap_or_else(|| self.i32_const(ctx, 0));
            self.store(ctx, MemKind::I32, transfer, transfer::AREA, area);
            self.store(ctx, MemKind::I32, transfer, transfer::OWNERS, owners);
            self.store(ctx, MemKind::I32, transfer, transfer::KIND, kind);
            let scope = self
                .resource_scope
                .map(|(scope, _)| scope)
                .unwrap_or_else(|| self.i32_const(ctx, 0));
            self.store(ctx, MemKind::I32, transfer, transfer::SCOPE, scope);
            self.publish_roots(ctx, &[transfer]);
            Some(transfer)
        } else {
            None
        };
        let name = if import.asynchronous {
            format!("[async-lower]{}", import.name)
        } else {
            import.name.clone()
        };
        let foreign = Foreign {
            module: import.module.clone(),
            name,
            params: signature.params.clone(),
        };
        self.commit_resources(ctx);
        if let Some(transfer) = transfer {
            // An async lowering returns its subtask status.
            let status = self.preserving_task(ctx, |b, ctx| {
                b.host(ctx, ValueHost::Foreign(foreign, WasmType::I32), args)
            });
            self.canonical_borrows = false;
            self.resource_scope = None;
            let ty = perry_mir::TransferType::get(ctx).into();
            let promise = self.new_promise(ctx, ty)?;
            let mask = self.i32_const(ctx, 15);
            let state = self.num(ctx, NumOpcode::I32And, vec![status, mask]);
            let two = self.i32_const(ctx, 2);
            let returned = self.num(ctx, NumOpcode::I32Eq, vec![state, two]);
            let now = self.new_block(ctx, vec![]);
            let later = self.new_block(ctx, vec![]);
            let done = self.new_block(ctx, vec![]);
            let bits = self.slot_bits(ctx, transfer);
            self.cond_br(ctx, returned, (now, vec![]), (later, vec![]));
            self.block = now;
            self.runtime_void(ctx, Runtime::PromiseResolve, vec![promise, bits]);
            self.br(ctx, done, vec![]);
            self.block = later;
            let four = self.i32_const(ctx, 4);
            let handle = self.num(ctx, NumOpcode::I32ShrU, vec![status, four]);
            self.runtime_void(ctx, Runtime::ForeignRegister, vec![handle, promise, bits]);
            self.runtime_void(ctx, Runtime::TaskJoin, vec![handle]);
            self.br(ctx, done, vec![]);
            self.block = done;
            self.define(ctx, op, promise);
        } else {
            // A synchronous lowering returns one flat result directly; none, or more through
            // `area`.
            let result = self.preserving_task(ctx, |b, ctx| match signature.results.as_slice() {
                &[flat] => Some(b.host(ctx, ValueHost::Foreign(foreign, flat), args)),
                _ => {
                    b.host_void(ctx, EffectHost::Foreign(foreign), args);
                    None
                }
            });
            self.canonical_borrows = false;
            if let Some(abi) = &import.result {
                let mir = op.deref(ctx).get_result(0).get_type(ctx);
                let value = match area {
                    Some(area) => self.lift_abi(ctx, abi, mir, Canonical::Memory(area, 0))?,
                    None => {
                        self.lift_abi(ctx, abi, mir, Canonical::Flat(&mut result.as_slice()))?
                    }
                };
                self.define(ctx, op, value);
            }
        }
        self.finish_resources(ctx);
        self.reset_roots(ctx, mark);
        Ok(())
    }
    fn finish_transfer_scope(&mut self, ctx: &mut Context, transfer: Value) {
        let scope = self.load(ctx, MemKind::I32, transfer, transfer::SCOPE);
        let done = self.enter_if(ctx, scope);
        self.runtime_void(ctx, Runtime::ResourceScopeFinish, vec![scope]);
        let zero = self.i32_const(ctx, 0);
        self.store(ctx, MemKind::I32, transfer, transfer::SCOPE, zero);
        self.drain_resources(ctx);
        self.br(ctx, done, vec![]);
        self.block = done;
    }
    pub(crate) fn foreign_finish(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        binding: &str,
    ) -> Result<()> {
        let import = self.state.bindings.get(binding).copied().ok_or_else(|| {
            diag!(
                codes::CONTRACT_MISMATCH,
                "WIT import {binding} is absent from the selected world"
            )
        })?;
        let transfer = self.operand(ctx, op, 0)?;
        let mir = op.deref(ctx).get_result(0).get_type(ctx);
        let value = if let Some(abi) = &import.result {
            let area = self.load(ctx, MemKind::I32, transfer, transfer::AREA);
            self.lift_abi(ctx, abi, mir, Canonical::Memory(area, 0))?
        } else {
            self.undefined(ctx, mir)?
        };
        let zero = self.i32_const(ctx, 0);
        self.store(ctx, MemKind::I32, transfer, transfer::AREA, zero);
        self.store(ctx, MemKind::I32, transfer, transfer::OWNERS, zero);
        self.finish_transfer_scope(ctx, transfer);
        self.define(ctx, op, value);
        Ok(())
    }

    /// Release canonical result payload buffers before detaching the transfer when acknowledged
    /// cancellation races a returned result.
    pub(super) fn cleanup_foreign(&mut self, ctx: &mut Context) -> Result<()> {
        if self.state.foreign_transfers.is_empty() {
            return Ok(());
        }
        let next = self.new_block(ctx, vec![]);
        let dispose = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.br(ctx, next, vec![]);
        self.block = next;
        let transfer = self.runtime(ctx, Runtime::ForeignCancelled, vec![]);
        self.cond_br(ctx, transfer, (dispose, vec![]), (done, vec![]));
        self.block = dispose;
        let returned = self.load(ctx, MemKind::I32, transfer, transfer::RETURNED);
        let clear = self.enter_if(ctx, returned);
        let kind = self.load(ctx, MemKind::I32, transfer, transfer::KIND);
        let area = self.load(ctx, MemKind::I32, transfer, transfer::AREA);
        for index in 0..self.state.foreign_transfers.len() {
            let import = self.state.foreign_transfers[index];
            let index = self.i32_const(ctx, index as i32);
            let matches = self.num(ctx, NumOpcode::I32Eq, vec![kind, index]);
            let other = self.enter_if(ctx, matches);
            if let Some(abi) = &import.result {
                self.discard_canonical(ctx, abi, area, 0)?;
            }
            self.br(ctx, clear, vec![]);
            self.block = other;
        }
        self.trap(ctx);
        self.block = clear;
        self.finish_transfer_scope(ctx, transfer);
        let zero = self.i32_const(ctx, 0);
        self.store(ctx, MemKind::I32, transfer, transfer::AREA, zero);
        self.store(ctx, MemKind::I32, transfer, transfer::OWNERS, zero);
        self.br(ctx, next, vec![]);
        self.block = done;
        Ok(())
    }

    fn discard_canonical(
        &mut self,
        ctx: &mut Context,
        abi: &AbiType,
        area: Value,
        offset: u32,
    ) -> Result<()> {
        match &abi.kind {
            AbiKind::String | AbiKind::List(_) => {
                let data = self.load(ctx, MemKind::I32, area, offset);
                if let AbiKind::List(element) = &abi.kind {
                    let len = self.load(ctx, MemKind::I32, area, offset + 4);
                    self.list_elements(ctx, element, data, len, |b, ctx, _, element_area| {
                        b.discard_canonical(ctx, element, element_area, 0)
                    })?;
                }
                self.runtime_void(ctx, Runtime::CabiRelease, vec![data]);
            }
            AbiKind::Record(fields) => {
                for (_, field) in fields {
                    let at = field.offset;
                    self.discard_canonical(ctx, field, area, offset + at)?;
                }
            }
            AbiKind::Sum(sum) => {
                let tag = self.load(ctx, sum.discriminant(), area, offset);
                self.sum_switch(ctx, tag, sum.cases(), vec![], |b, ctx, _, (_, payload)| {
                    if let Some(payload) = payload {
                        b.discard_canonical(ctx, payload, area, offset + payload.offset)?;
                    }
                    Ok(vec![])
                })?;
            }
            AbiKind::Resource { key, owned: true } => {
                let handle = self.load(ctx, MemKind::I32, area, offset);
                self.discard_resource(ctx, *key, handle);
            }
            AbiKind::ByteStream => {
                bail_unsupported!("resource-bearing async imports need an ownership binding")
            }
            _ => {}
        }
        Ok(())
    }
}
