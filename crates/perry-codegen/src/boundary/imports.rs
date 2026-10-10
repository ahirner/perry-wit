//! Canonical lowering of statically bound host calls.
use super::*;
use perry_runtime::foreign as transfer;
use pliron::{context::Ptr, operation::Operation, r#type::Typed};

impl FunctionLowering<'_> {
    pub(crate) fn foreign_call(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        binding: &str,
    ) -> Result<()> {
        let import = self
            .state
            .bindings
            .get(binding)
            .cloned()
            .ok_or_else(|| anyhow!("WIT import {binding} is absent from the selected world"))?;
        if let ImportKind::Resource { key, operation } = import.kind {
            return self.resource_helper(ctx, op, key, operation);
        }
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        ensure!(
            operands.len() == import.params.len(),
            "WIT import argument count mismatch"
        );
        let values = self.operands(ctx, op)?;
        let mark = self
            .call(ctx, "perry_roots_mark", &[], &[crate::W::I32], vec![])
            .expect("roots");
        for (mir, value) in operands.iter().zip(&values) {
            if crate::repr(ctx, mir.get_type(ctx))? == crate::Repr::Ref {
                self.call(ctx, "perry_roots_push", &[crate::W::I32], &[], vec![*value]);
            }
        }
        self.begin_resources(
            ctx,
            import.params.iter().any(AbiType::has_resources),
            Some(mark),
        );
        self.canonical_borrows = true;
        let mut args = Vec::new();
        let mut params: Vec<_> = import.params.iter().flat_map(AbiType::flat).collect();
        if params.len()
            > if import.asynchronous {
                4
            } else {
                MAX_FLAT_PARAMS
            }
        {
            let aggregate = parameters::record(&import.params);
            let size = self.i32_const(ctx, aggregate.size() as i32);
            let area = self
                .runtime(ctx, Runtime::Alloc, vec![size])
                .expect("arguments");
            for (((abi, mir), value), offset) in import
                .params
                .iter()
                .zip(&operands)
                .zip(&values)
                .zip(aggregate.field_offsets())
            {
                self.lower_store(ctx, abi, mir.get_type(ctx), *value, area, offset)?;
            }
            args.push(area);
            params = vec![Flat::I32];
        } else {
            for ((abi, mir), value) in import.params.iter().zip(&operands).zip(&values) {
                self.lower_flat_values(ctx, abi, mir.get_type(ctx), *value, &mut args)?;
            }
        }
        let result_flat = import
            .result
            .as_ref()
            .map(AbiType::flat)
            .unwrap_or_default();
        let area = if result_flat.len() > 1 || (import.asynchronous && import.result.is_some()) {
            let size = self.i32_const(ctx, import.result.as_ref().unwrap().size() as i32);
            let area = self
                .runtime(ctx, Runtime::Alloc, vec![size])
                .expect("result area");
            args.push(area);
            params.push(Flat::I32);
            Some(area)
        } else {
            None
        };
        let transfer = if import.asynchronous {
            let owners = self
                .call(
                    ctx,
                    "perry_roots_capture",
                    &[crate::W::I32],
                    &[crate::W::I32],
                    vec![mark],
                )
                .expect("owners");
            let size = self.i32_const(ctx, transfer::SIZE as i32);
            let shape = self.pointer_shape(&[transfer::AREA, transfer::OWNERS]);
            let shape = self.i32_const(ctx, shape as i32);
            let transfer = self
                .call(
                    ctx,
                    "perry_gc_alloc",
                    &[crate::W::I32; 2],
                    &[crate::W::I32],
                    vec![size, shape],
                )
                .expect("transfer");
            let index = if let Some(index) = self
                .state
                .foreign_transfers
                .iter()
                .position(|name| name == binding)
            {
                index
            } else {
                self.state.foreign_transfers.push(binding.to_owned());
                self.state.foreign_transfers.len() - 1
            };
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
            self.call(
                ctx,
                "perry_roots_push",
                &[crate::W::I32],
                &[],
                vec![transfer],
            );
            Some(transfer)
        } else {
            None
        };
        let results = if import.asynchronous {
            vec![Flat::I32]
        } else if area.is_some() {
            vec![]
        } else {
            result_flat
        };
        let name = if import.asynchronous {
            format!("[async-lower]{}", import.name)
        } else {
            import.name.clone()
        };
        let host = crate::Host::Foreign(import.module, name, params, results.clone());
        self.state.host.insert(host.clone());
        let result_types = results.iter().map(|flat| flat_type(ctx, *flat)).collect();
        self.commit_resources(ctx);
        let task = self
            .call(ctx, "perry_task_current", &[], &[crate::W::I32], vec![])
            .expect("task");
        let call = CallOp::new(ctx, host.symbol(), args, result_types).get_operation();
        self.insert(ctx, call);
        self.runtime(ctx, Runtime::TaskResume, vec![task]);
        self.canonical_borrows = false;
        if let Some(transfer) = transfer {
            self.resource_scope = None;
            let ty = perry_mir::types::TransferType::get(ctx).into();
            let promise = self.new_promise(ctx, ty)?;
            let status = call.deref(ctx).get_result(0);
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
            self.runtime(ctx, Runtime::PromiseResolve, vec![promise, bits]);
            self.br(ctx, done, vec![]);
            self.block = later;
            let four = self.i32_const(ctx, 4);
            let handle = self.num(ctx, NumOpcode::I32ShrU, vec![status, four]);
            self.call(
                ctx,
                "perry_foreign_register",
                &[crate::W::I32, crate::W::I32, crate::W::I64],
                &[],
                vec![handle, promise, bits],
            );
            self.runtime(ctx, Runtime::TaskJoin, vec![handle]);
            self.br(ctx, done, vec![]);
            self.block = done;
            self.define(ctx, op, promise);
        } else if let Some(abi) = &import.result {
            let mir = op.deref(ctx).get_result(0).get_type(ctx);
            let value = if let Some(area) = area {
                self.lift_load(ctx, abi, mir, area, 0)?
            } else {
                let mut values = call.deref(ctx).results().collect::<Vec<_>>();
                values.reverse();
                self.lift_flat(ctx, abi, mir, &mut values)?
            };
            self.define(ctx, op, value);
        }
        self.finish_resources(ctx);
        self.call(ctx, "perry_roots_reset", &[crate::W::I32], &[], vec![mark]);
        Ok(())
    }
    fn finish_transfer_scope(&mut self, ctx: &mut Context, transfer: Value) {
        let scope = self.load(ctx, MemKind::I32, transfer, transfer::SCOPE);
        let release = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, scope, (release, vec![]), (done, vec![]));
        self.block = release;
        self.call(
            ctx,
            "perry_resource_scope_finish",
            &[crate::W::I32],
            &[],
            vec![scope],
        );
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
        let import = self.state.bindings[binding].clone();
        let transfer = self.operand(ctx, op, 0)?;
        let mir = op.deref(ctx).get_result(0).get_type(ctx);
        let value = if let Some(abi) = &import.result {
            let area = self.load(ctx, MemKind::I32, transfer, transfer::AREA);
            self.lift_load(ctx, abi, mir, area, 0)?
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

    /// Acknowledged cancellation can race a returned canonical result. Release its payload
    /// buffers before detaching the transfer; pending callbacks can no longer access them.
    pub(super) fn cleanup_foreign(&mut self, ctx: &mut Context) -> Result<()> {
        if self.state.foreign_transfers.is_empty() {
            return Ok(());
        }
        let next = self.new_block(ctx, vec![]);
        let dispose = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.br(ctx, next, vec![]);
        self.block = next;
        let transfer = self
            .call(
                ctx,
                "perry_foreign_cancelled",
                &[],
                &[crate::W::I32],
                vec![],
            )
            .expect("cancelled transfer");
        self.cond_br(ctx, transfer, (dispose, vec![]), (done, vec![]));
        self.block = dispose;
        let returned = self.load(ctx, MemKind::I32, transfer, transfer::RETURNED);
        let result = self.new_block(ctx, vec![]);
        let clear = self.new_block(ctx, vec![]);
        self.cond_br(ctx, returned, (result, vec![]), (clear, vec![]));
        self.block = result;
        let kind = self.load(ctx, MemKind::I32, transfer, transfer::KIND);
        let area = self.load(ctx, MemKind::I32, transfer, transfer::AREA);
        let bindings = self.state.foreign_transfers.clone();
        for (index, binding) in bindings.iter().enumerate() {
            let selected = self.new_block(ctx, vec![]);
            let other = self.new_block(ctx, vec![]);
            let index = self.i32_const(ctx, index as i32);
            let matches = self.num(ctx, NumOpcode::I32Eq, vec![kind, index]);
            self.cond_br(ctx, matches, (selected, vec![]), (other, vec![]));
            self.block = selected;
            if let Some(abi) = self.state.bindings[binding].result.clone() {
                self.discard_canonical(ctx, &abi, area, 0)?;
            }
            self.br(ctx, clear, vec![]);
            self.block = other;
        }
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
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
        match abi {
            AbiType::String | AbiType::List(_) => {
                let data = self.load(ctx, MemKind::I32, area, offset);
                if let AbiType::List(element) = abi {
                    let len = self.load(ctx, MemKind::I32, area, offset + 4);
                    self.counted_loop(ctx, len, |b, ctx, index| {
                        let stride = b.i32_const(ctx, element.size() as i32);
                        let position = b.num(ctx, NumOpcode::I32Mul, vec![index, stride]);
                        let element_area = b.num(ctx, NumOpcode::I32Add, vec![data, position]);
                        b.discard_canonical(ctx, element, element_area, 0)
                    })?;
                }
                self.call(ctx, "perry_cabi_release", &[crate::W::I32], &[], vec![data]);
            }
            AbiType::Record(fields) => {
                for ((_, field), at) in fields.iter().zip(abi.field_offsets()) {
                    self.discard_canonical(ctx, field, area, offset + at)?;
                }
            }
            AbiType::Option(_)
            | AbiType::Result { .. }
            | AbiType::Enum(_)
            | AbiType::Variant(_) => {
                let tag = self.load(ctx, abi.discriminant().mem_kind().unwrap(), area, offset);
                self.sum_switch(ctx, tag, abi.cases().len(), vec![], |b, ctx, case| {
                    if let Some(payload) = abi.cases()[case] {
                        b.discard_canonical(ctx, payload, area, offset + abi.payload_offset())?;
                    }
                    Ok(vec![])
                })?;
            }
            AbiType::Resource { key, owned: true } => {
                let handle = self.load(ctx, MemKind::I32, area, offset);
                self.discard_resource(ctx, *key, handle);
            }
            AbiType::ByteStream | AbiType::OkHandle { .. } => {
                bail!("resource-bearing async imports need an ownership binding")
            }
            _ => {}
        }
        Ok(())
    }
}
