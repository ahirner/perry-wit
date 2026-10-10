//! Explicit, checked JSON schema conversion into ordinary native aggregates.
use super::*;

impl FunctionBuilder<'_> {
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
        let existing = self.shapes.decoders.borrow().get(&target).cloned();
        let name = match existing {
            Some(name) => name,
            None => {
                let mut index = self.shapes.decoders.borrow().len();
                let name = loop {
                    let name = identifier(&format!("perry_json_decode_{index}"))?;
                    if self
                        .signatures
                        .values()
                        .all(|signature| signature.name != name)
                        && self
                            .shapes
                            .decoders
                            .borrow()
                            .values()
                            .all(|other| *other != name)
                    {
                        break name;
                    }
                    index += 1;
                };
                self.shapes
                    .decoders
                    .borrow_mut()
                    .insert(target, name.clone());
                let signature = Signature {
                    name: name.clone(),
                    is_async: false,
                    params: vec![JsonType::get(ctx).into()],
                    results: vec![target],
                };
                let ty =
                    FunctionType::get(ctx, signature.params.clone(), signature.results.clone());
                let function = FuncOp::new(ctx, name.clone(), ty);
                self.shapes
                    .module
                    .append_operation(ctx, function.get_operation(), 0);
                let input = function.get_entry_block(ctx).deref(ctx).get_argument(0);
                let mut builder = Self::new(
                    ctx,
                    &function,
                    &signature,
                    self.signatures,
                    self.shapes,
                    self.builtins,
                );
                let output = builder.decode_fields(ctx, input, target)?;
                builder.emit(ctx, |ctx| ReturnOp::new(ctx, vec![output]));
                remove_unreachable(ctx, builder.region);
                name
            }
        };
        let call = CallOp::new(ctx, name, vec![value], vec![target]);
        self.append(ctx, &call);
        Ok(call.get_operation().deref(ctx).get_result(0))
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
            return Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)));
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
