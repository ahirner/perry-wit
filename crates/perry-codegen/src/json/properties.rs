use super::*;
use perry_abi::json::{NEXT, PACKED_FIELDS, array_index};

impl FunctionLowering<'_, '_> {
    /// Native values and retained extra members share one ordered field dispatch.
    pub(super) fn json_record_members(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        ty: TypeHandle,
        keys: &[String],
        types: &[TypeHandle],
    ) -> Result<()> {
        let i32 = i32_type(ctx);
        let zero = self.i32_const(ctx, 0);
        let one = self.i32_const(ctx, 1);
        let count = self.i32_const(ctx, types.len() as i32);
        let order = self.load(ctx, MemKind::I32, value, record::ORDER);
        let graph = if self.state.retains_json {
            self.load(ctx, MemKind::I32, value, record::JSON)
        } else {
            zero
        };
        let first = if self.state.retains_json {
            self.runtime(ctx, Runtime::StdJsonMembers, vec![graph])
        } else {
            zero
        };
        let mut offset = record::HEADER;
        let offsets = types
            .iter()
            .map(|&ty| {
                let current = offset;
                offset += slot_size(ctx, ty);
                current
            })
            .collect::<Vec<_>>();
        let mut numeric = keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| array_index(key.as_bytes()).map(|number| (number, index)))
            .collect::<Vec<_>>();
        numeric.sort_unstable();
        if !numeric.is_empty() {
            let emit = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            self.cond_br(ctx, graph, (next, vec![]), (emit, vec![]));
            self.block = emit;
            for &(_, index) in &numeric {
                self.json_record_field(
                    ctx,
                    writer,
                    value,
                    &keys[index],
                    types[index],
                    offsets[index],
                )?;
            }
            self.br(ctx, next, vec![]);
            self.block = next;
        }
        let packed = types.len() <= PACKED_FIELDS;
        let header = self.new_block(ctx, vec![i32, i32]);
        let ordered = self.new_block(ctx, vec![]);
        let retained = self.new_block(ctx, vec![]);
        let dispatch = self.new_block(ctx, vec![i32, i32, i32, i32]);
        let done = self.new_block(ctx, vec![]);
        self.br(ctx, header, vec![if packed { order } else { zero }, first]);
        self.block = header;
        let position = header.deref(ctx).get_argument(0);
        let member = header.deref(ctx).get_argument(1);
        self.cond_br(ctx, graph, (retained, vec![]), (ordered, vec![]));

        self.block = ordered;
        let body = self.new_block(ctx, vec![]);
        let active = if packed { position } else { order };
        self.cond_br(ctx, active, (body, vec![]), (done, vec![]));
        self.block = body;
        let (index, next) = if packed {
            let mask = self.i32_const(ctx, 15);
            let index = self.num(ctx, NumOpcode::I32And, vec![position, mask]);
            let index = self.num(ctx, NumOpcode::I32Sub, vec![index, one]);
            let shift = self.i32_const(ctx, 4);
            let next = self.num(ctx, NumOpcode::I32ShrU, vec![position, shift]);
            (index, next)
        } else {
            let length = self.load(ctx, MemKind::I32, order, 0);
            let has_item = self.num(ctx, NumOpcode::I32LtU, vec![position, length]);
            let item = self.new_block(ctx, vec![]);
            self.cond_br(ctx, has_item, (item, vec![]), (done, vec![]));
            self.block = item;
            let shift = self.i32_const(ctx, 2);
            let offset = self.num(ctx, NumOpcode::I32Shl, vec![position, shift]);
            let address = self.num(ctx, NumOpcode::I32Add, vec![order, offset]);
            let index = self.load(ctx, MemKind::I32, address, 4);
            let next = self.num(ctx, NumOpcode::I32Add, vec![position, one]);
            (index, next)
        };
        self.br(ctx, dispatch, vec![index, next, zero, zero]);

        self.block = retained;
        let body = self.new_block(ctx, vec![]);
        self.cond_br(ctx, member, (body, vec![]), (done, vec![]));
        self.block = body;
        let (table, _) = self.json_field_table(ctx, ty)?;
        let table = self.i32_const(ctx, table as i32);
        let index = self.runtime(ctx, Runtime::StdJsonRecordIndex, vec![member, table, count]);
        let next = self.load(ctx, MemKind::I32, member, NEXT as u32);
        self.br(ctx, dispatch, vec![index, zero, next, member]);

        self.block = dispatch;
        let index = dispatch.deref(ctx).get_argument(0);
        let next_position = dispatch.deref(ctx).get_argument(1);
        let next_member = dispatch.deref(ctx).get_argument(2);
        let member = dispatch.deref(ctx).get_argument(3);
        let arms = keys
            .iter()
            .map(|_| self.new_block(ctx, vec![]))
            .collect::<Vec<_>>();
        let extra = self.new_block(ctx, vec![]);
        let next = self.new_block(ctx, vec![]);
        self.br_table(ctx, index, arms.clone(), extra);
        for (index, arm) in arms.into_iter().enumerate() {
            self.block = arm;
            if array_index(keys[index].as_bytes()).is_some() {
                let emit = self.new_block(ctx, vec![]);
                self.cond_br(ctx, graph, (emit, vec![]), (next, vec![]));
                self.block = emit;
            }
            self.json_record_field(
                ctx,
                writer,
                value,
                &keys[index],
                types[index],
                offsets[index],
            )?;
            self.br(ctx, next, vec![]);
        }
        self.block = extra;
        self.runtime_void(ctx, Runtime::StdJsonWriteMember, vec![writer, member]);
        self.br(ctx, next, vec![]);
        self.block = next;
        self.br(ctx, header, vec![next_position, next_member]);
        self.block = done;
        Ok(())
    }
}
