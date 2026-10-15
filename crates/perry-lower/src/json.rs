//! Explicit, checked JSON schema conversion into ordinary native aggregates.
use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn json_scalar_equal(
        &mut self,
        ctx: &mut Context,
        json: Value,
        scalar: Value,
    ) -> Result<Value> {
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        if self.is_optional(ctx, scalar) {
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, scalar));
            return self.choose(
                ctx,
                boolean,
                undefined,
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![json])),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, scalar));
                    b.choose(
                        ctx,
                        boolean,
                        null,
                        |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonIsNull, vec![json])),
                        |b, ctx| {
                            let scalar = b.defined(ctx, scalar)?;
                            b.json_scalar_equal(ctx, json, scalar)
                        },
                    )
                },
            );
        }
        if kind(ctx, scalar)? == Kind::Exception {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonExceptionEqual, vec![json, scalar]));
        }
        if matches!(kind(ctx, scalar)?, Kind::Record | Kind::Array) {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonOriginEqual, vec![json, scalar]));
        }
        let tag = match kind(ctx, scalar)? {
            Kind::String => "string",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            _ => bail!("JSON equality requires a string, number or boolean operand"),
        };
        let actual = self.intrinsic(ctx, Intrinsic::JsonTypeOf, vec![json]);
        let expected = self.string(ctx, tag);
        let matches = self.compare(ctx, CompareKind::Eq, actual, expected);
        self.choose(
            ctx,
            boolean,
            matches,
            |b, ctx| {
                let decoded = b.fit(ctx, json, scalar.get_type(ctx))?;
                Ok(b.compare(ctx, CompareKind::Eq, decoded, scalar))
            },
            |b, ctx| Ok(b.boolean(ctx, false)),
        )
    }

    pub(super) fn decode_aggregate(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        let name = self.json_decoder(
            ctx,
            target,
            crate::shapes::DecoderInput::Snapshot,
            |builder, ctx, entry| {
                let input = entry.deref(ctx).get_argument(0);
                Ok(vec![builder.decode_fields(ctx, input, target)?])
            },
        )?;
        let call = CallOp::new(ctx, name, vec![value], vec![target]);
        self.append(ctx, &call);
        Ok(call.get_operation().deref(ctx).get_result(0))
    }

    pub(super) fn json_decoder(
        &mut self,
        ctx: &mut Context,
        target: TypeHandle,
        input: crate::shapes::DecoderInput,
        build: impl FnOnce(&mut Self, &mut Context, Ptr<BasicBlock>) -> Result<Vec<Value>>,
    ) -> Result<Identifier> {
        use crate::shapes::DecoderInput;
        let key = (target, input);
        if let Some(name) = self.shapes.decoders.borrow().get(&key).cloned() {
            return Ok(name);
        }
        let (prefix, params, results) = match input {
            DecoderInput::Snapshot => ("decode", vec![JsonType::get(ctx).into()], vec![target]),
            DecoderInput::Text => ("parse", vec![StringType::get(ctx).into()], vec![target]),
            DecoderInput::Cursor => (
                "cursor",
                vec![
                    perry_mir::types::JsonCursorType::get(ctx).into(),
                    IntegerType::get(ctx, 32, Signedness::Signless).into(),
                ],
                vec![
                    target,
                    IntegerType::get(ctx, 1, Signedness::Signless).into(),
                ],
            ),
        };
        let mut index = self.shapes.decoders.borrow().len();
        let name = loop {
            let candidate = identifier(&format!("perry_json_{prefix}_{index}"))?;
            if self.signatures.values().all(|s| s.name != candidate)
                && self
                    .shapes
                    .decoders
                    .borrow()
                    .values()
                    .all(|n| *n != candidate)
            {
                break candidate;
            }
            index += 1;
        };
        self.shapes.decoders.borrow_mut().insert(key, name.clone());
        let signature = Signature {
            name: name.clone(),
            is_async: false,
            await_initialization: false,
            params: params.iter().copied().map(BindingType::plain).collect(),
            result: results.first().copied().map(BindingType::plain),
        };
        let ty = FunctionType::get(ctx, params, results);
        let function = FuncOp::new(ctx, name.clone(), ty);
        perry_mir::calls::set_json_decoder(
            ctx,
            &function,
            match input {
                DecoderInput::Text => perry_mir::attrs::JsonDecoderKind::Parse,
                DecoderInput::Snapshot => perry_mir::attrs::JsonDecoderKind::Value,
                DecoderInput::Cursor => perry_mir::attrs::JsonDecoderKind::Cursor,
            },
        );
        if input != DecoderInput::Snapshot {
            perry_mir::calls::set_atomic_construction(ctx, &function);
        }
        self.shapes
            .module
            .append_operation(ctx, function.get_operation(), 0);
        let entry = function.get_entry_block(ctx);
        let mut builder = Self::new(
            ctx,
            &function,
            &signature,
            self.signatures,
            self.shapes,
            self.builtins,
        );
        let results = build(&mut builder, ctx, entry)?;
        builder.emit(ctx, |ctx| ReturnOp::new(ctx, results));
        remove_unreachable(ctx, builder.region);
        Ok(name)
    }

    fn decode_fields(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        if target.deref(ctx).is::<perry_mir::types::NumberStringType>() {
            let tag = self.intrinsic(ctx, Intrinsic::JsonTypeOf, vec![value]);
            let number = self.string(ctx, "number");
            let is_number = self.compare(ctx, CompareKind::Eq, tag, number);
            return self.choose(
                ctx,
                target,
                is_number,
                |b, ctx| {
                    let ty = FP64Type::get(ctx).into();
                    let number = b.fit(ctx, value, ty)?;
                    b.fit(ctx, number, target)
                },
                |b, ctx| {
                    let ty = StringType::get(ctx).into();
                    let string = b.fit(ctx, value, ty)?;
                    b.fit(ctx, string, target)
                },
            );
        }
        if let Ok(variant) = TypedHandle::<perry_mir::types::VariantType>::from_handle(target, ctx)
        {
            let cases = variant.deref(ctx).cases().to_vec();
            self.intrinsic_void(ctx, Intrinsic::JsonRequireObject, vec![value]);
            let tag_key = self.string(ctx, "tag");
            let tag = self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, tag_key]);
            let string = StringType::get(ctx).into();
            let tag = self.fit(ctx, tag, string)?;
            let result = self.entry_slot(ctx, target);
            let done = self.new_block(ctx, "variant_done");
            for (name, payload) in cases {
                let text = self.string(ctx, &name);
                let matches = self.compare(ctx, CompareKind::Eq, tag, text);
                let active = self.new_block(ctx, "variant_case");
                let next = self.new_block(ctx, "variant_next");
                self.branch(ctx, matches, active, next);
                self.switch_to(active);
                let payload = if let Some(ty) = payload {
                    let key = self.string(ctx, "val");
                    let member = self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, key]);
                    Some(self.fit(ctx, member, ty)?)
                } else {
                    None
                };
                let decoded = self.emit_value(ctx, |ctx| {
                    perry_mir::variants::VariantNewOp::new(ctx, target, &name, payload)
                });
                self.intrinsic_void(ctx, Intrinsic::JsonRetain, vec![decoded, value]);
                self.store(ctx, result, decoded);
                self.jump(ctx, done);
                self.switch_to(next);
            }
            let message = self.string(ctx, "JSON tag is not a case of the declared variant");
            let error = self.emit_value(ctx, |ctx| {
                ErrorNewOp::new(ctx, ErrorKind::TypeError, message)
            });
            let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, error));
            self.propagate(ctx, status);
            self.switch_to(done);
            return self.load(ctx, result);
        }
        if let Ok(record) = TypedHandle::<RecordType>::from_handle(target, ctx) {
            let tuple = record.deref(ctx).is_tuple();
            let fields = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("opaque JSON schema"))?
                .to_vec();
            let mut element = if tuple {
                self.call_intrinsic(ctx, Intrinsic::JsonFirst, vec![value])
            } else {
                self.intrinsic_void(ctx, Intrinsic::JsonRequireObject, vec![value]);
                None
            };
            let mut values = Vec::with_capacity(fields.len());
            for (field, ty) in fields {
                let member = if let Some(current) = element {
                    element = self.call_intrinsic(ctx, Intrinsic::JsonNext, vec![current]);
                    current
                } else {
                    let name = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, field.as_ref()));
                    self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, name])
                };
                values.push(self.fit(ctx, member, ty)?);
            }
            let decoded = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values));
            self.intrinsic_void(ctx, Intrinsic::JsonRetain, vec![decoded, value]);
            return Ok(decoded);
        }
        if let Some((k_ty, v_ty, tuple_rec)) = crate::collections::dict_entry_types(ctx, target) {
            return self.decode_dict(ctx, value, k_ty, v_ty, tuple_rec);
        }
        if let Ok(array_type) = TypedHandle::<ArrayType>::from_handle(target, ctx) {
            let element = array_type.deref(ctx).element();
            let first = self.intrinsic(ctx, Intrinsic::JsonFirst, vec![value]);
            let json = JsonType::get(ctx).into();
            let cursor = self.entry_slot(ctx, json);
            self.emit(ctx, |ctx| StoreOp::new(ctx, cursor, first));
            let array = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
            let header = self.new_block(ctx, "decode_header");
            let body = self.new_block(ctx, "decode_element");
            let done = self.new_block(ctx, "decode_done");
            self.jump(ctx, header);
            self.switch_to(header);
            let load = LoadOp::new(ctx, cursor)?;
            let item = self.append_value(ctx, &load);
            let end = self.intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![item]);
            self.branch(ctx, end, done, body);
            self.switch_to(body);
            let decoded = self.fit(ctx, item, element)?;
            self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, decoded));
            let next = self.intrinsic(ctx, Intrinsic::JsonNext, vec![item]);
            self.emit(ctx, |ctx| StoreOp::new(ctx, cursor, next));
            self.jump(ctx, header);
            self.switch_to(done);
            return Ok(array);
        }
        bail!(
            "JSON schema requires scalars, closed records or dense arrays, found {}",
            target.disp(ctx)
        )
    }
}

impl<'s> FunctionBuilder<'s> {
    /// An object literal with spreads, which Perry HIR lowers to a source-ordered IIFE over
    /// `{}`: `js_object_literal_spread` copies each spread and `js_object_literal_define`
    /// defines each static member. Over JSON values it builds a JSON object; typed records
    /// do not spread yet.
    pub(super) fn object_spread(
        &mut self,
        ctx: &mut Context,
        target: LocalId,
        body: &'s [Stmt],
    ) -> Result<Value> {
        const UNSUPPORTED: &str = "object spread is supported over JSON values (JSON.parse, \
            response.json()) with static keys; computed keys, methods and accessors are not";
        let object = self.intrinsic(ctx, Intrinsic::JsonObject, vec![]);
        let is_target = |expr: &'s Expr| matches!(expr, Expr::LocalGet(id) if *id == target);
        for statement in body {
            match statement {
                Stmt::Let {
                    id,
                    init: Some(value),
                    ..
                } => {
                    let value = self.json_value(ctx, value)?;
                    let slot = self.entry_slot(ctx, value.get_type(ctx));
                    self.store(ctx, slot, value);
                    self.locals.insert(*id, LocalBinding::plain(slot));
                }
                Stmt::Expr(Expr::Call { callee, args, .. })
                    if matches!(&**callee, Expr::ExternFuncRef { name, .. }
                        if name == "js_object_literal_spread")
                        && args.len() == 2
                        && is_target(&args[0]) =>
                {
                    let source = self.expression(ctx, &args[1])?;
                    // An `Error` has no own enumerable properties: it adds nothing.
                    if matches!(kind(ctx, source), Ok(Kind::Exception)) {
                        continue;
                    }
                    let Some(source) = self.json_operand(ctx, source)? else {
                        bail!("{UNSUPPORTED}");
                    };
                    self.intrinsic_void(ctx, Intrinsic::JsonAssign, vec![object, source]);
                }
                Stmt::Expr(Expr::Call { callee, args, .. })
                    if matches!(&**callee, Expr::ExternFuncRef { name, .. }
                        if name == "js_object_literal_define")
                        && args.len() == 3
                        && is_target(&args[0]) =>
                {
                    let Expr::String(key) = &args[1] else {
                        bail!("{UNSUPPORTED}");
                    };
                    let key = self.string(ctx, key);
                    let value = self.json_value(ctx, &args[2])?;
                    self.intrinsic_void(ctx, Intrinsic::JsonSet, vec![object, key, value]);
                }
                Stmt::Return(Some(result)) if is_target(result) => {}
                _ => bail!("{UNSUPPORTED}"),
            }
        }
        Ok(object)
    }

    /// A JSON value, or `Json | undefined` (an array element) as one: the JSON `undefined` is
    /// the null address. `None` for other types.
    pub(super) fn json_operand(
        &mut self,
        ctx: &mut Context,
        value: Value,
    ) -> Result<Option<Value>> {
        let ty = value.get_type(ctx);
        if ty.deref(ctx).is::<JsonType>() {
            return Ok(Some(value));
        }
        if !optional_value(ctx, ty).is_some_and(|inner| inner.deref(ctx).is::<JsonType>()) {
            return Ok(None);
        }
        let json = JsonType::get(ctx).into();
        let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
        self.choose(
            ctx,
            json,
            undefined,
            |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
            |b, ctx| b.defined(ctx, value),
        )
        .map(Some)
    }

    /// Retain supported Error causes, preserving reference identity alongside their readable fields.
    pub(super) fn error_cause(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(json) = self.json_operand(ctx, value)? {
            return Ok(json);
        }
        if self.is_optional(ctx, value) {
            let json = JsonType::get(ctx).into();
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            return self.choose(
                ctx,
                json,
                undefined,
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose(
                        ctx,
                        json,
                        null,
                        |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonNull, vec![])),
                        |b, ctx| {
                            let value = b.defined(ctx, value)?;
                            b.error_cause(ctx, value)
                        },
                    )
                },
            );
        }
        let intrinsic = match kind(ctx, value)? {
            Kind::Number => Intrinsic::JsonFromNumber,
            Kind::String => Intrinsic::JsonFromString,
            Kind::Boolean => Intrinsic::JsonFromBool,
            Kind::Exception => Intrinsic::JsonFromException,
            Kind::Record | Kind::Array => {
                return Ok(self.emit_value(ctx, |ctx| {
                    perry_mir::collections::NativeJsonOp::new(ctx, value)
                }));
            }
            _ => bail!("unsupported Error cause type"),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }

    /// A member value of a JSON object literal: a JSON value, or a number, string, boolean
    /// or `null` converted to one.
    fn json_value(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        if matches!(expr, Expr::Null) {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonNull, vec![]));
        }
        let value = self.expression(ctx, expr)?;
        if let Some(json) = self.json_operand(ctx, value)? {
            return Ok(json);
        }
        let intrinsic = match kind(ctx, value)? {
            Kind::Number => Intrinsic::JsonFromNumber,
            Kind::String => Intrinsic::JsonFromString,
            Kind::Boolean => Intrinsic::JsonFromBool,
            _ => bail!(
                "members of a spread object literal are JSON values, numbers, strings, \
                 booleans or null so far"
            ),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }
}

/// Schemas supported by direct cursor construction with the current runtime layout.
/// Other schema kinds use checked snapshot conversion.
pub(super) fn direct_json_schema(ctx: &Context, root: TypeHandle) -> bool {
    let mut pending = vec![root];
    let mut visited = std::collections::HashSet::new();
    while let Some(ty) = pending.pop() {
        if !visited.insert(ty) {
            continue;
        }
        if crate::collections::dict_entry_types(ctx, ty).is_some()
            || crate::collections::map_entry_types(ctx, ty).is_some()
            || crate::collections::set_entry_type(ctx, ty).is_some()
        {
            return false;
        }
        let ty = ty.deref(ctx);
        if let Some(record) = ty.downcast_ref::<RecordType>() {
            let Ok(fields) = record.fields() else {
                return false;
            };
            pending.extend(fields.iter().map(|(_, ty)| *ty));
        } else if let Some(array) = ty.downcast_ref::<ArrayType>() {
            pending.push(array.element());
        } else if let Some(variant) = ty.downcast_ref::<perry_mir::types::VariantType>() {
            pending.extend(variant.cases().iter().filter_map(|(_, ty)| *ty));
        } else if let Some(optional) = ty.downcast_ref::<perry_mir::types::OptionalType>() {
            pending.push(optional.value());
        } else if !(ty.is::<FP64Type>()
            || ty.is::<StringType>()
            || ty.is::<JsonType>()
            || ty.is::<perry_mir::types::NumberStringType>()
            || ty
                .downcast_ref::<IntegerType>()
                .is_some_and(|int| matches!(int.width(), 1 | 64)))
        {
            return false;
        }
    }
    true
}
