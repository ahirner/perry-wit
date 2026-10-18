//! Schema-specific JSON construction in MIR over the shared JSON lexer.
use super::*;
use perry_mir::{JsonCursorKeyOp, NumberStringType};

impl<'s> FunctionBuilder<'s> {
    fn cursor_index(&mut self, ctx: &mut Context, n: u32) -> Value {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(n), bw(32)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.append_value(ctx, &op)
    }
    fn cursor_consume(&mut self, ctx: &mut Context, cursor: Value, byte: u8) {
        let byte = self.cursor_index(ctx, u32::from(byte));
        self.intrinsic_void(ctx, Intrinsic::JsonCursorConsume, vec![cursor, byte]);
    }
    fn cursor_is(&mut self, ctx: &mut Context, byte: Value, expected: u8) -> Value {
        let expected = self.cursor_index(ctx, u32::from(expected));
        self.compare(ctx, CompareKind::Eq, byte, expected)
    }
    fn cursor_or(&mut self, ctx: &mut Context, a: Value, b: Value) -> Result<Value> {
        let ty = a.get_type(ctx);
        self.choose(ctx, ty, a, |b, ctx| Ok(b.boolean(ctx, true)), |_, _| Ok(b))
    }
    fn cursor_bad(&mut self, ctx: &mut Context, cursor: Value, ty: TypeHandle) -> (Value, Value) {
        self.intrinsic_void(ctx, Intrinsic::JsonCursorDiscard, vec![cursor]);
        let value = self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty));
        let failed = self.boolean(ctx, true);
        (value, failed)
    }
    fn cursor_choice(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        condition: Value,
        yes: impl FnOnce(&mut Self, &mut Context) -> Result<(Value, Value)>,
        no: impl FnOnce(&mut Self, &mut Context) -> Result<(Value, Value)>,
    ) -> Result<(Value, Value)> {
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let value = self.entry_slot(ctx, ty);
        let failed = self.entry_slot(ctx, boolean);
        let then = self.new_block(ctx, "json_match");
        let other = self.new_block(ctx, "json_mismatch");
        let done = self.new_block(ctx, "json_value");
        self.branch(ctx, condition, then, other);
        self.switch_to(then);
        let (v, e) = yes(self, ctx)?;
        self.store(ctx, value, v);
        self.store(ctx, failed, e);
        self.jump(ctx, done);
        self.switch_to(other);
        let (v, e) = no(self, ctx)?;
        self.store(ctx, value, v);
        self.store(ctx, failed, e);
        self.jump(ctx, done);
        self.switch_to(done);
        Ok((self.load(ctx, value)?, self.load(ctx, failed)?))
    }
    pub(super) fn parse_cursor(
        &mut self,
        ctx: &mut Context,
        text: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let name = self.json_decoder(
            ctx,
            ty,
            crate::shapes::DecoderInput::Text,
            |b, ctx, entry| {
                let input = entry.deref(ctx).get_argument(0);
                let cursor = b.intrinsic(ctx, Intrinsic::JsonCursorBegin, vec![input]);
                let (value, failed) = b.cursor_value(ctx, cursor, ty)?;
                b.intrinsic_void(ctx, Intrinsic::JsonCursorFinish, vec![cursor, failed]);
                Ok(vec![value])
            },
        )?;
        let call = CallOp::new(ctx, name, vec![text], vec![ty]);
        self.append(ctx, &call);
        Ok(call.get_operation().deref(ctx).get_result(0))
    }

    fn cursor_value(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        ty: TypeHandle,
    ) -> Result<(Value, Value)> {
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        self.cursor_value_at(ctx, cursor, ty, byte)
    }
    fn cursor_value_at(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        ty: TypeHandle,
        byte: Value,
    ) -> Result<(Value, Value)> {
        if let Some(inner) = optional_value(ctx, ty) {
            let null = self.cursor_is(ctx, byte, b'n');
            let admitted = absence(ctx, ty).is_some_and(|s| s.admits_null());
            return self.cursor_choice(
                ctx,
                ty,
                null,
                |b, ctx| {
                    if admitted {
                        b.intrinsic_void(ctx, Intrinsic::JsonCursorDiscard, vec![cursor]);
                        let null = b.emit_value(ctx, |ctx| NullOp::new(ctx, ty));
                        let e = b.boolean(ctx, false);
                        Ok((null, e))
                    } else {
                        Ok(b.cursor_bad(ctx, cursor, ty))
                    }
                },
                |b, ctx| {
                    let (v, e) = b.cursor_value_at(ctx, cursor, inner, byte)?;
                    let v = b.fit(ctx, v, ty)?;
                    Ok((v, e))
                },
            );
        }
        if ty.deref(ctx).is::<JsonType>() {
            let value = self.intrinsic(ctx, Intrinsic::JsonCursorValue, vec![cursor]);
            let e = self.boolean(ctx, false);
            return Ok((value, e));
        }
        if ty.deref(ctx).is::<NumberStringType>() {
            let string = self.cursor_is(ctx, byte, b'"');
            return self.cursor_choice(
                ctx,
                ty,
                string,
                |b, ctx| {
                    let string = StringType::get(ctx).into();
                    let (v, e) = b.cursor_value_at(ctx, cursor, string, byte)?;
                    let v = b.fit(ctx, v, ty)?;
                    Ok((v, e))
                },
                |b, ctx| {
                    let number = FP64Type::get(ctx).into();
                    let (v, e) = b.cursor_value_at(ctx, cursor, number, byte)?;
                    let v = b.fit(ctx, v, ty)?;
                    Ok((v, e))
                },
            );
        }
        if ty.deref(ctx).is::<RecordType>()
            || ty.deref(ctx).is::<ArrayType>()
            || ty.deref(ctx).is::<perry_mir::VariantType>()
        {
            let name = self.json_decoder(
                ctx,
                ty,
                crate::shapes::DecoderInput::Cursor,
                |b, ctx, entry| {
                    let input = entry.deref(ctx).get_argument(0);
                    let byte = entry.deref(ctx).get_argument(1);
                    let variant = ty.deref(ctx).is::<perry_mir::VariantType>();
                    let array = ty.deref(ctx).is::<ArrayType>()
                        || TypedHandle::<RecordType>::from_handle(ty, ctx)
                            .is_ok_and(|r| r.deref(ctx).is_tuple());
                    let wanted = b.cursor_is(ctx, byte, if array { b'[' } else { b'{' });
                    let (value, failed) = b.cursor_choice(
                        ctx,
                        ty,
                        wanted,
                        |b, ctx| {
                            if array {
                                b.cursor_array(ctx, input, ty)
                            } else if variant {
                                b.cursor_variant(ctx, input, ty)
                            } else {
                                b.cursor_record(ctx, input, ty)
                            }
                        },
                        |b, ctx| Ok(b.cursor_bad(ctx, input, ty)),
                    )?;
                    Ok(vec![value, failed])
                },
            )?;
            let call = CallOp::new(
                ctx,
                name,
                vec![cursor, byte],
                vec![ty, IntegerType::get(ctx, 1, Signedness::Signless).into()],
            );
            self.append(ctx, &call);
            let op = call.get_operation().deref(ctx);
            return Ok((op.get_result(0), op.get_result(1)));
        }
        let int64_signed = ty
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .filter(|int| int.width() == 64)
            .map(|int| int.is_signed());
        let (matches, intrinsic) = if ty.deref(ctx).is::<FP64Type>() {
            let minus = self.cursor_is(ctx, byte, b'-');
            let zero = self.cursor_index(ctx, u32::from(b'0'));
            let nine = self.cursor_index(ctx, u32::from(b'9'));
            let lower = self.compare(ctx, CompareKind::Ge, byte, zero);
            let upper = self.compare(ctx, CompareKind::Le, byte, nine);
            let boolean = lower.get_type(ctx);
            let digits = self.choose(
                ctx,
                boolean,
                lower,
                |_, _| Ok(upper),
                |b, ctx| Ok(b.boolean(ctx, false)),
            )?;
            (
                self.cursor_or(ctx, minus, digits)?,
                Intrinsic::JsonCursorNumber,
            )
        } else if let Some(signed) = int64_signed {
            let minus = self.cursor_is(ctx, byte, b'-');
            let zero = self.cursor_index(ctx, u32::from(b'0'));
            let nine = self.cursor_index(ctx, u32::from(b'9'));
            let lower = self.compare(ctx, CompareKind::Ge, byte, zero);
            let upper = self.compare(ctx, CompareKind::Le, byte, nine);
            let boolean = lower.get_type(ctx);
            let digits = self.choose(
                ctx,
                boolean,
                lower,
                |_, _| Ok(upper),
                |b, ctx| Ok(b.boolean(ctx, false)),
            )?;
            (
                self.cursor_or(ctx, minus, digits)?,
                if signed {
                    Intrinsic::JsonCursorI64
                } else {
                    Intrinsic::JsonCursorU64
                },
            )
        } else if ty.deref(ctx).is::<StringType>() {
            (self.cursor_is(ctx, byte, b'"'), Intrinsic::JsonCursorString)
        } else if self.is_bool_type(ctx, ty) {
            let t = self.cursor_is(ctx, byte, b't');
            let f = self.cursor_is(ctx, byte, b'f');
            (self.cursor_or(ctx, t, f)?, Intrinsic::JsonCursorBoolean)
        } else {
            bail!("unsupported cursor scalar {}", ty.disp(ctx))
        };
        self.cursor_choice(
            ctx,
            ty,
            matches,
            |b, ctx| {
                let v = b.intrinsic(ctx, intrinsic, vec![cursor]);
                let e = if int64_signed.is_some() {
                    let query = b.cursor_index(ctx, u32::MAX - 1);
                    let failed = b.intrinsic(ctx, Intrinsic::JsonCursorSeek, vec![cursor, query]);
                    let zero = b.cursor_index(ctx, 0);
                    b.compare(ctx, CompareKind::Ne, failed, zero)
                } else {
                    b.boolean(ctx, false)
                };
                Ok((v, e))
            },
            |b, ctx| Ok(b.cursor_bad(ctx, cursor, ty)),
        )
    }
    /// The value of a member missing from the input and whether its absence fails decoding.
    /// Types admitting `undefined` take it (`omitted` for record fields, so `in` sees no key),
    /// `unknown` members take JSON `undefined` as JS property access would, others fail.
    fn cursor_absent(&mut self, ctx: &mut Context, ty: TypeHandle, omitted: bool) -> (Value, bool) {
        if absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
            let value = if omitted {
                self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty))
            } else {
                self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
            };
            (value, false)
        } else if ty.deref(ctx).is::<JsonType>() {
            (self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![]), false)
        } else {
            (self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)), true)
        }
    }

    fn cursor_dispatch(
        &mut self,
        ctx: &mut Context,
        index: Value,
        start: u32,
        arms: &[Ptr<BasicBlock>],
    ) -> Result<()> {
        if arms.len() == 1 {
            self.jump(ctx, arms[0]);
            return Ok(());
        }
        let mid = arms.len() / 2;
        let boundary = self.cursor_index(ctx, start + mid as u32);
        let lower = self.compare(ctx, CompareKind::Lt, index, boundary);
        let left = self.new_block(ctx, "json_dispatch_low");
        let right = self.new_block(ctx, "json_dispatch_high");
        self.branch(ctx, lower, left, right);
        self.switch_to(left);
        self.cursor_dispatch(ctx, index, start, &arms[..mid])?;
        self.switch_to(right);
        self.cursor_dispatch(ctx, index, start + mid as u32, &arms[mid..])
    }

    fn cursor_record(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        ty: TypeHandle,
    ) -> Result<(Value, Value)> {
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx)?;
        let fields = record.deref(ctx).fields()?.to_vec();
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let mut values = Vec::with_capacity(fields.len());
        let mut failures = Vec::with_capacity(fields.len());
        for (_, field) in &fields {
            let slot = self.entry_slot(ctx, *field);
            let (absent, missing) = self.cursor_absent(ctx, *field, true);
            self.store(ctx, slot, absent);
            values.push(slot);
            let e = self.entry_slot(ctx, boolean);
            let missing = self.boolean(ctx, missing);
            self.store(ctx, e, missing);
            failures.push(e);
        }
        let json = JsonType::get(ctx).into();
        let residue = self.entry_slot(ctx, json);
        let empty = self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![]);
        self.store(ctx, residue, empty);
        self.cursor_consume(ctx, cursor, b'{');
        let header = self.new_block(ctx, "json_members");
        let body = self.new_block(ctx, "json_member");
        let step = self.new_block(ctx, "json_delimiter");
        let done = self.new_block(ctx, "json_object_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b'}');
        self.branch(ctx, end, done, body);
        self.switch_to(body);
        let key = self.emit_value(ctx, |ctx| JsonCursorKeyOp::new(ctx, cursor, ty));
        let mut arms = fields
            .iter()
            .map(|_| self.new_block(ctx, "json_field"))
            .collect::<Vec<_>>();
        let unknown = self.new_block(ctx, "json_unknown");
        arms.push(unknown);
        self.cursor_dispatch(ctx, key, 0, &arms)?;
        for (i, (_, field)) in fields.iter().enumerate() {
            self.switch_to(arms[i]);
            let (v, e) = self.cursor_value(ctx, cursor, *field)?;
            self.store(ctx, values[i], v);
            self.store(ctx, failures[i], e);
            self.jump(ctx, step);
        }
        self.switch_to(unknown);
        let graph = self.load(ctx, residue)?;
        let graph = self.intrinsic(ctx, Intrinsic::JsonCursorMember, vec![cursor, graph]);
        self.store(ctx, residue, graph);
        self.jump(ctx, step);
        self.switch_to(step);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b'}');
        let more = self.new_block(ctx, "json_more");
        self.branch(ctx, end, done, more);
        self.switch_to(more);
        self.cursor_consume(ctx, cursor, b',');
        self.jump(ctx, body);
        self.switch_to(done);
        self.cursor_consume(ctx, cursor, b'}');
        let mut failed = self.boolean(ctx, false);
        for e in failures {
            let e = self.load(ctx, e)?;
            failed = self.cursor_or(ctx, failed, e)?;
        }
        let fields = values
            .into_iter()
            .map(|v| self.load(ctx, v))
            .collect::<Result<Vec<_>>>()?;
        let native = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, fields));
        let graph = self.load(ctx, residue)?;
        self.intrinsic_void(ctx, Intrinsic::JsonResidue, vec![native, graph]);
        Ok((native, failed))
    }
    fn cursor_array(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        ty: TypeHandle,
    ) -> Result<(Value, Value)> {
        if let Ok(record) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            return self.cursor_tuple(ctx, cursor, record);
        }
        let array = TypedHandle::<ArrayType>::from_handle(ty, ctx)?;
        let element = array.deref(ctx).element();
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let failed = self.entry_slot(ctx, boolean);
        let no = self.boolean(ctx, false);
        self.store(ctx, failed, no);
        let value = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        self.cursor_consume(ctx, cursor, b'[');
        let header = self.new_block(ctx, "json_elements");
        let body = self.new_block(ctx, "json_element");
        let done = self.new_block(ctx, "json_array_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b']');
        self.branch(ctx, end, done, body);
        self.switch_to(body);
        let (v, e) = self.cursor_value(ctx, cursor, element)?;
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, value, v));
        let old = self.load(ctx, failed)?;
        let e = self.cursor_or(ctx, old, e)?;
        self.store(ctx, failed, e);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b']');
        let more = self.new_block(ctx, "json_array_more");
        self.branch(ctx, end, done, more);
        self.switch_to(more);
        self.cursor_consume(ctx, cursor, b',');
        self.jump(ctx, body);
        self.switch_to(done);
        self.cursor_consume(ctx, cursor, b']');
        Ok((value, self.load(ctx, failed)?))
    }
    fn cursor_tuple(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        record: TypedHandle<RecordType>,
    ) -> Result<(Value, Value)> {
        let fields = record.deref(ctx).fields()?.to_vec();
        let mut values = Vec::with_capacity(fields.len());
        let mut failed = self.boolean(ctx, false);
        self.cursor_consume(ctx, cursor, b'[');
        for (_, ty) in &fields {
            let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
            let end = self.cursor_is(ctx, byte, b']');
            let (v, e) = self.cursor_choice(
                ctx,
                *ty,
                end,
                |b, ctx| {
                    let (v, failed) = b.cursor_absent(ctx, *ty, false);
                    Ok((v, b.boolean(ctx, failed)))
                },
                |b, ctx| {
                    let pair = b.cursor_value(ctx, cursor, *ty)?;
                    let byte = b.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
                    let comma = b.cursor_is(ctx, byte, b',');
                    let yes = b.new_block(ctx, "tuple_comma");
                    let no = b.new_block(ctx, "tuple_next");
                    b.branch(ctx, comma, yes, no);
                    b.switch_to(yes);
                    b.cursor_consume(ctx, cursor, b',');
                    b.jump(ctx, no);
                    b.switch_to(no);
                    Ok(pair)
                },
            )?;
            values.push(v);
            failed = self.cursor_or(ctx, failed, e)?;
        }
        let json = JsonType::get(ctx).into();
        let residue = self.entry_slot(ctx, json);
        let empty = self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![]);
        self.store(ctx, residue, empty);
        let header = self.new_block(ctx, "tuple_tail");
        let body = self.new_block(ctx, "tuple_extra");
        let done = self.new_block(ctx, "tuple_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b']');
        self.branch(ctx, end, done, body);
        self.switch_to(body);
        let graph = self.load(ctx, residue)?;
        let start = self.cursor_index(ctx, fields.len() as u32);
        let graph = self.intrinsic(ctx, Intrinsic::JsonCursorTail, vec![cursor, graph, start]);
        self.store(ctx, residue, graph);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b']');
        let more = self.new_block(ctx, "tuple_tail_more");
        self.branch(ctx, end, done, more);
        self.switch_to(more);
        self.cursor_consume(ctx, cursor, b',');
        self.jump(ctx, body);
        self.switch_to(done);
        self.cursor_consume(ctx, cursor, b']');
        let native = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values));
        let graph = self.load(ctx, residue)?;
        self.intrinsic_void(ctx, Intrinsic::JsonResidue, vec![native, graph]);
        Ok((native, failed))
    }
    fn cursor_variant(
        &mut self,
        ctx: &mut Context,
        cursor: Value,
        ty: TypeHandle,
    ) -> Result<(Value, Value)> {
        let cases = perry_mir::variant_cases(ctx, ty)?;
        let string_ty: TypeHandle = StringType::get(ctx).into();
        let boolean: TypeHandle = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let index_ty: TypeHandle = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let json_ty: TypeHandle = JsonType::get(ctx).into();

        let tag_slot = self.entry_slot(ctx, string_ty);
        let undef_tag = self.emit_value(ctx, |ctx| UndefOp::new(ctx, string_ty));
        self.store(ctx, tag_slot, undef_tag);

        let tag_failed = self.entry_slot(ctx, boolean);
        let yes = self.boolean(ctx, true);
        self.store(ctx, tag_failed, yes);

        let val_case = self.entry_slot(ctx, index_ty);
        let missing_case = self.cursor_index(ctx, u32::MAX);
        self.store(ctx, val_case, missing_case);

        let val_failed = self.entry_slot(ctx, boolean);
        let no = self.boolean(ctx, false);
        self.store(ctx, val_failed, no);

        let deferred_pos = self.entry_slot(ctx, index_ty);
        let zero = self.cursor_index(ctx, 0);
        self.store(ctx, deferred_pos, zero);

        let residue = self.entry_slot(ctx, json_ty);
        let empty = self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![]);
        self.store(ctx, residue, empty);

        let mut payload_slots = Vec::with_capacity(cases.len());
        for (_, payload) in &cases {
            if let Some(payload_ty) = *payload {
                let slot = self.entry_slot(ctx, payload_ty);
                let undef = self.emit_value(ctx, |ctx| UndefOp::new(ctx, payload_ty));
                self.store(ctx, slot, undef);
                payload_slots.push(Some(slot));
            } else {
                payload_slots.push(None);
            }
        }

        self.cursor_consume(ctx, cursor, b'{');
        let header = self.new_block(ctx, "json_variant_members");
        let body = self.new_block(ctx, "json_variant_member");
        let step = self.new_block(ctx, "json_variant_delimiter");
        let done = self.new_block(ctx, "json_variant_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b'}');
        self.branch(ctx, end, done, body);

        self.switch_to(body);
        let key = self.emit_value(ctx, |ctx| JsonCursorKeyOp::new(ctx, cursor, ty));
        let arm_tag = self.new_block(ctx, "json_variant_tag");
        let arm_val = self.new_block(ctx, "json_variant_val");
        let arm_unknown = self.new_block(ctx, "json_variant_unknown");
        self.cursor_dispatch(ctx, key, 0, &[arm_tag, arm_val, arm_unknown])?;

        self.switch_to(arm_tag);
        let (v, e) = self.cursor_value(ctx, cursor, string_ty)?;
        self.store(ctx, tag_slot, v);
        self.store(ctx, tag_failed, e);
        self.jump(ctx, step);

        self.switch_to(arm_val);
        let mark_arg = self.cursor_index(ctx, u32::MAX);
        let pos = self.intrinsic(ctx, Intrinsic::JsonCursorSeek, vec![cursor, mark_arg]);
        self.store(ctx, deferred_pos, pos);
        let no_tag = self.load(ctx, tag_failed)?;
        let dispatch_val = self.new_block(ctx, "json_variant_val_dispatch");
        let defer_val = self.new_block(ctx, "json_variant_val_defer");
        self.branch(ctx, no_tag, defer_val, dispatch_val);

        self.switch_to(dispatch_val);
        let current_tag = self.load(ctx, tag_slot)?;
        for (case_i, (name, payload)) in cases.iter().enumerate() {
            let Some(payload_ty) = *payload else {
                continue;
            };
            let expected = self.string(ctx, name);
            let matched = self.compare(ctx, CompareKind::Eq, current_tag, expected);
            let hit = self.new_block(ctx, "json_variant_val_hit");
            let miss = self.new_block(ctx, "json_variant_val_miss");
            self.branch(ctx, matched, hit, miss);
            self.switch_to(hit);
            let (v, e) = self.cursor_value(ctx, cursor, payload_ty)?;
            if let Some(slot) = payload_slots[case_i] {
                self.store(ctx, slot, v);
            }
            self.store(ctx, val_failed, e);
            let idx = self.cursor_index(ctx, case_i as u32);
            self.store(ctx, val_case, idx);
            self.jump(ctx, step);
            self.switch_to(miss);
        }
        self.jump(ctx, defer_val);

        self.switch_to(defer_val);
        let graph = self.load(ctx, residue)?;
        let graph = self.intrinsic(ctx, Intrinsic::JsonCursorMember, vec![cursor, graph]);
        self.store(ctx, residue, graph);
        let deferred_marker = self.cursor_index(ctx, u32::MAX - 1);
        self.store(ctx, val_case, deferred_marker);
        self.jump(ctx, step);

        self.switch_to(arm_unknown);
        let graph = self.load(ctx, residue)?;
        let graph = self.intrinsic(ctx, Intrinsic::JsonCursorMember, vec![cursor, graph]);
        self.store(ctx, residue, graph);
        self.jump(ctx, step);

        self.switch_to(step);
        let byte = self.intrinsic(ctx, Intrinsic::JsonCursorByte, vec![cursor]);
        let end = self.cursor_is(ctx, byte, b'}');
        let more = self.new_block(ctx, "json_variant_more");
        self.branch(ctx, end, done, more);
        self.switch_to(more);
        self.cursor_consume(ctx, cursor, b',');
        self.jump(ctx, body);

        self.switch_to(done);
        let result_slot = self.entry_slot(ctx, ty);
        let undef_result = self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty));
        self.store(ctx, result_slot, undef_result);
        let failed_slot = self.entry_slot(ctx, boolean);
        let yes = self.boolean(ctx, true);
        self.store(ctx, failed_slot, yes);

        let finish = self.new_block(ctx, "json_variant_finish");
        let check_cases = self.new_block(ctx, "json_variant_cases");
        let no_tag = self.load(ctx, tag_failed)?;
        self.branch(ctx, no_tag, finish, check_cases);

        self.switch_to(check_cases);
        let final_tag = self.load(ctx, tag_slot)?;
        let vc = self.load(ctx, val_case)?;
        for (case_i, (name, payload)) in cases.iter().enumerate() {
            let expected = self.string(ctx, name);
            let matched = self.compare(ctx, CompareKind::Eq, final_tag, expected);
            let case_block = self.new_block(ctx, "json_variant_case");
            let next_case = self.new_block(ctx, "json_variant_next");
            self.branch(ctx, matched, case_block, next_case);
            self.switch_to(case_block);
            match *payload {
                None => {
                    let decoded = self
                        .emit_value(ctx, |ctx| perry_mir::VariantNewOp::new(ctx, ty, name, None));
                    let graph = self.load(ctx, residue)?;
                    self.intrinsic_void(ctx, Intrinsic::JsonResidue, vec![decoded, graph]);
                    let ok = self.boolean(ctx, false);
                    self.store(ctx, result_slot, decoded);
                    self.store(ctx, failed_slot, ok);
                    self.jump(ctx, finish);
                }
                Some(payload_ty) => {
                    let idx = self.cursor_index(ctx, case_i as u32);
                    let already = self.compare(ctx, CompareKind::Eq, vc, idx);
                    let slot = payload_slots[case_i];
                    let (payload_val, payload_err) = self.cursor_choice(
                        ctx,
                        payload_ty,
                        already,
                        |b, ctx| {
                            let v = match slot {
                                Some(slot) => b.load(ctx, slot)?,
                                None => b.emit_value(ctx, |ctx| UndefOp::new(ctx, payload_ty)),
                            };
                            Ok((v, b.load(ctx, val_failed)?))
                        },
                        |b, ctx| {
                            let missing_idx = b.cursor_index(ctx, u32::MAX);
                            let is_missing = b.compare(ctx, CompareKind::Eq, vc, missing_idx);
                            b.cursor_choice(
                                ctx,
                                payload_ty,
                                is_missing,
                                |b, ctx| {
                                    let (v, failed) = b.cursor_absent(ctx, payload_ty, false);
                                    Ok((v, b.boolean(ctx, failed)))
                                },
                                |b, ctx| {
                                    let pos = b.load(ctx, deferred_pos)?;
                                    let saved = b.intrinsic(
                                        ctx,
                                        Intrinsic::JsonCursorSeek,
                                        vec![cursor, pos],
                                    );
                                    let (v, e) = b.cursor_value(ctx, cursor, payload_ty)?;
                                    let _ = b.intrinsic(
                                        ctx,
                                        Intrinsic::JsonCursorSeek,
                                        vec![cursor, saved],
                                    );
                                    Ok((v, e))
                                },
                            )
                        },
                    )?;
                    let build_ok = self.new_block(ctx, "json_variant_build");
                    self.branch(ctx, payload_err, finish, build_ok);
                    self.switch_to(build_ok);
                    let decoded = self.emit_value(ctx, |ctx| {
                        perry_mir::VariantNewOp::new(ctx, ty, name, Some(payload_val))
                    });
                    let graph = self.load(ctx, residue)?;
                    self.intrinsic_void(ctx, Intrinsic::JsonResidue, vec![decoded, graph]);
                    let ok = self.boolean(ctx, false);
                    self.store(ctx, result_slot, decoded);
                    self.store(ctx, failed_slot, ok);
                    self.jump(ctx, finish);
                }
            }
            self.switch_to(next_case);
        }
        self.jump(ctx, finish);

        self.switch_to(finish);
        self.cursor_consume(ctx, cursor, b'}');
        Ok((self.load(ctx, result_slot)?, self.load(ctx, failed_slot)?))
    }
}
