//! `Record<K, V>` dictionaries and closed records: `Object.*`, `Array.isArray`, `in`, `delete`.

use super::*;

impl<'s> FunctionBuilder<'s> {
    /// A read of an optional that admits `undefined`, normalized to the type's `undefined`.
    pub(crate) fn normalize_optional_read(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        let ty = val.get_type(ctx);
        if optional_value(ctx, ty).is_some()
            && absence(ctx, ty).is_some_and(|a| a.admits_undefined())
        {
            let is_undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, val));
            let undef = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
            return self.choose(ctx, ty, is_undef, |_, _| Ok(undef), |_, _| Ok(val));
        }
        Ok(val)
    }

    /// Lower an object literal `{ k1: v1, ... }` when expected type is `Record<K, V>`.
    pub(crate) fn dict_literal(
        &mut self,
        ctx: &mut Context,
        names: &[&str],
        args: &'s [Expr],
        v_ty: TypeHandle,
        tuple_rec: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let dict = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, tuple_rec.into(), vec![]));
        for (name, val_expr) in names.iter().zip(args) {
            let k = self.string(ctx, name);
            let v = self.expression_with(ctx, val_expr, Some(v_ty))?;
            self.kv_set(ctx, dict, k, v)?;
        }
        Ok(dict)
    }

    /// Dynamic string indexing `rec[key]` on a closed `RecordType` when all fields share a uniform type.
    pub(crate) fn record_dynamic_get(
        &mut self,
        ctx: &mut Context,
        rec: Value,
        key: Value,
        rec_handle: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let uniform_ty = record_uniform_field_type(ctx, rec_handle).context(
            "dynamic indexing on a closed record requires all fields to share the same type, or use Record<string, V>",
        )?;
        let fields = rec_handle.deref(ctx).fields()?.to_vec();
        let opt_ty = crate::coerce::admitting(ctx, uniform_ty, perry_mir::Absence::Undefined);
        let mut acc = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty));
        for (name, _) in fields.into_iter().rev() {
            let name_str = self.string(ctx, &name);
            let eq = self.compare(ctx, CompareKind::Eq, key, name_str);
            let op = FieldGetOp::new(ctx, rec, name)?;
            let field_val = self.append_value(ctx, &op);
            let normalized = self.normalize_optional_read(ctx, field_val)?;
            let field_opt = self.fit(ctx, normalized, opt_ty)?;
            acc = self.choose(ctx, opt_ty, eq, |_, _| Ok(field_opt), |_, _| Ok(acc))?;
        }
        Ok(acc)
    }

    /// Closed-record enumeration shares presence checks and value normalization.
    pub(super) fn object_members(
        &mut self,
        ctx: &mut Context,
        obj_expr: &'s Expr,
        method: IteratorMethod,
    ) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if dict_entry_types(ctx, ty).is_some() {
            return self.collection_iterator(ctx, obj, method);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let str_ty: TypeHandle = StringType::get(ctx).into();
            let elem_ty = if method == IteratorMethod::Keys {
                str_ty
            } else {
                record_uniform_field_type(ctx, rec).with_context(|| format!(
                    "Object.{} on a closed record requires all fields to share a uniform value type",
                    method.name(),
                ))?
            };
            let pair = if method == IteratorMethod::Entries {
                Some(self.shapes.pair_type(ctx, str_ty, elem_ty)?)
            } else {
                None
            };
            let output_ty = pair.map_or(elem_ty, Into::into);
            let fields = rec.deref(ctx).fields()?.to_vec();
            let out = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, output_ty, vec![]));
            for (name, field_ty) in fields {
                let after = if admits_omission(ctx, field_ty) {
                    let op = FieldPresentOp::new(ctx, obj, name.clone())?;
                    let present = self.append_value(ctx, &op);
                    let keep = self.new_block(ctx, "obj_member_keep");
                    let after = self.new_block(ctx, "obj_member_after");
                    self.branch(ctx, present, keep, after);
                    self.switch_to(keep);
                    Some(after)
                } else {
                    None
                };
                let value = if method == IteratorMethod::Keys {
                    self.string(ctx, &name)
                } else {
                    let entry = pair.map(|pair| (pair, self.string(ctx, &name)));
                    let op = FieldGetOp::new(ctx, obj, name)?;
                    let value = self.append_value(ctx, &op);
                    let value = if admits_omission(ctx, field_ty) {
                        self.normalize_optional_read(ctx, value)?
                    } else {
                        value
                    };
                    let value = self.fit(ctx, value, elem_ty)?;
                    if let Some((pair, key)) = entry {
                        self.emit_value(ctx, |ctx| AllocOp::new(ctx, pair, vec![key, value]))
                    } else {
                        value
                    }
                };
                self.emit(ctx, |ctx| ArrayPushOp::new(ctx, out, value));
                if let Some(after) = after {
                    self.jump(ctx, after);
                    self.switch_to(after);
                }
            }
            return Ok(out);
        }
        if method == IteratorMethod::Keys {
            if let Some(json) = self.json_operand(ctx, obj)? {
                return Ok(self.intrinsic(ctx, Intrinsic::JsonKeys, vec![json]));
            }
            bail!("Object.keys requires a Record, interface/object shape, or JSON value");
        }
        bail!(
            "Object.{} requires a Record<K, V> or uniform record shape",
            method.name()
        )
    }

    /// `Object.fromEntries(entries)`.
    pub(super) fn object_from_entries(
        &mut self,
        ctx: &mut Context,
        entries_expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let entries_val = self.expression_with(ctx, entries_expr, expected)?;
        let entries_val = self.defined(ctx, entries_val)?;
        let (k_ty, v_ty) = iterable_kv_types(ctx, entries_val.get_type(ctx))
            .or_else(|| expected.and_then(|e| iterable_kv_types(ctx, e)))
            .context("Object.fromEntries requires an iterable of [key, value] pairs")?;
        let dict_tuple = match expected.and_then(|e| dict_entry_types(ctx, e).map(|(_, _, r)| r)) {
            Some(r) => r,
            None => self.shapes.mir_record_prefix(
                ctx,
                vec![("_0".to_string(), k_ty), ("_1".to_string(), v_ty)],
                Some("dict_tuple"),
                RecordKind::DictionaryEntry,
            )?,
        };
        let dict = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, dict_tuple.into(), vec![]));
        self.live_collection_loop(ctx, "from_entries", entries_val, |b, ctx, _, entry| {
            let (k, v) = b.entry_fields(ctx, entry)?;
            b.kv_set(ctx, dict, k, v)?;
            Ok(())
        })?;
        Ok(dict)
    }

    /// `Object.is(a, b)` — SameValue equality (`NaN` equals `NaN`, `+0` does not equal `-0`).
    pub(super) fn object_is(
        &mut self,
        ctx: &mut Context,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Value> {
        let lhs = self.expression(ctx, left)?;
        let rhs = self.expression(ctx, right)?;
        if lhs.get_type(ctx).deref(ctx).is::<FP64Type>()
            && rhs.get_type(ctx).deref(ctx).is::<FP64Type>()
        {
            let eq = self.compare(ctx, CompareKind::Eq, lhs, rhs);
            let bool_ty = eq.get_type(ctx);
            let a_nan = self.compare(ctx, CompareKind::Ne, lhs, lhs);
            let b_nan = self.compare(ctx, CompareKind::Ne, rhs, rhs);
            let both_nan = self.choose(ctx, bool_ty, a_nan, |_, _| Ok(b_nan), |_, _| Ok(a_nan))?;
            let zero = self.number(ctx, 0.0);
            let is_zero = self.compare(ctx, CompareKind::Eq, lhs, zero);
            let one = self.number(ctx, 1.0);
            let inv_a = self.binary(ctx, BinaryKind::Div, one, lhs);
            let inv_b = self.binary(ctx, BinaryKind::Div, one, rhs);
            let same_sign_zero = self.compare(ctx, CompareKind::Eq, inv_a, inv_b);
            let eq_non_neg_zero = self.choose(
                ctx,
                bool_ty,
                is_zero,
                |_, _| Ok(same_sign_zero),
                |b, ctx| Ok(b.boolean(ctx, true)),
            )?;
            return self.choose(
                ctx,
                bool_ty,
                eq,
                |_, _| Ok(eq_non_neg_zero),
                |_, _| Ok(both_nan),
            );
        }
        self.comparison(ctx, &CompareOp::Eq, left, right)
    }

    /// `Array.isArray(val)`.
    pub(super) fn array_is_array(
        &mut self,
        ctx: &mut Context,
        val_expr: &'s Expr,
    ) -> Result<Value> {
        let val = self.expression(ctx, val_expr)?;
        let ty = val.get_type(ctx);
        let inner = optional_value(ctx, ty).unwrap_or(ty);
        let is_plain_array = inner.deref(ctx).is::<ArrayType>()
            && map_entry_types(ctx, inner).is_none()
            && dict_entry_types(ctx, inner).is_none()
            && set_entry_type(ctx, inner).is_none();
        if ty.deref(ctx).is::<OptionalType>() {
            if !is_plain_array {
                return Ok(self.boolean(ctx, false));
            }
            let nullish = self.is_nullish(ctx, val)?;
            let false_val = self.boolean(ctx, false);
            return Ok(self.compare(ctx, CompareKind::Eq, nullish, false_val));
        }
        Ok(self.boolean(ctx, is_plain_array))
    }

    /// `key in obj`.
    pub(crate) fn in_operator(
        &mut self,
        ctx: &mut Context,
        prop_expr: &'s Expr,
        obj_expr: &'s Expr,
    ) -> Result<Value> {
        let obj = self.expression(ctx, obj_expr)?;
        let obj = self.defined(ctx, obj)?;
        let ty = obj.get_type(ctx);
        if let Some((k_ty, _, _)) = dict_entry_types(ctx, ty) {
            let k = self.expression_with(ctx, prop_expr, Some(k_ty))?;
            return self.kv_has(ctx, obj, k, k_ty);
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let k = self.string_operand(ctx, prop_expr)?;
            let fields = rec
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .to_vec();
            let mut acc = self.boolean(ctx, false);
            let bool_ty = acc.get_type(ctx);
            for (name, field_ty) in fields {
                let name_val = self.string(ctx, &name);
                let eq = self.compare(ctx, CompareKind::Eq, k, name_val);
                acc = self.choose(
                    ctx,
                    bool_ty,
                    eq,
                    |b, ctx| {
                        if admits_omission(ctx, field_ty) {
                            let op = FieldPresentOp::new(ctx, obj, name)?;
                            Ok(b.append_value(ctx, &op))
                        } else {
                            Ok(b.boolean(ctx, true))
                        }
                    },
                    |_, _| Ok(acc),
                )?;
            }
            return Ok(acc);
        }
        bail!("'in' operator requires a Record<K, V> or closed record shape")
    }

    /// `delete obj[key]` or `delete obj.prop` on a `Record<K, V>` dictionary or optional record field.
    pub(super) fn delete_operator(&mut self, ctx: &mut Context, inner: &'s Expr) -> Result<Value> {
        match inner {
            Expr::IndexGet { object, index } => {
                let obj = self.expression(ctx, object)?;
                let obj = self.defined(ctx, obj)?;
                if let Some((k_ty, _, _)) = dict_entry_types(ctx, obj.get_type(ctx)) {
                    let k = self.expression_with(ctx, index, Some(k_ty))?;
                    self.kv_delete(ctx, obj, k, k_ty)?;
                    return Ok(self.boolean(ctx, true));
                }
                bail!("delete operator is only supported on Record<K, V> dictionary types")
            }
            Expr::PropertyGet {
                object, property, ..
            } => {
                let obj = self.expression(ctx, object)?;
                let obj = self.defined(ctx, obj)?;
                let ty = obj.get_type(ctx);
                if let Some((k_ty, _, _)) = dict_entry_types(ctx, ty) {
                    let k = self.string(ctx, property);
                    self.kv_delete(ctx, obj, k, k_ty)?;
                    return Ok(self.boolean(ctx, true));
                }
                let opt_field = TypedHandle::<RecordType>::from_handle(ty, ctx)
                    .ok()
                    .and_then(|rec| rec.deref(ctx).field(property).map(|(_, t)| t));
                if let Some(field_ty) = opt_field
                    && admits_omission(ctx, field_ty)
                {
                    let omitted = self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, field_ty));
                    self.emit(ctx, |ctx| {
                        FieldSetOp::new(ctx, obj, property.clone(), omitted)
                    });
                    return Ok(self.boolean(ctx, true));
                }
                bail!(
                    "delete operator is only supported on Record<K, V> dictionary types or optional record fields"
                )
            }
            _ => bail!("delete operator requires a property or index access on a Record<K, V>"),
        }
    }
}

/// Whether a record field of type `ty` may be omitted (an optional admitting `undefined`).
fn admits_omission(ctx: &Context, ty: TypeHandle) -> bool {
    optional_value(ctx, ty).is_some() && absence(ctx, ty).is_some_and(|a| a.admits_undefined())
}
