//! Typed `JSON.parse` in one pass: decoders generated per declared type pull values from
//! perry-std's reader (`json_read`) straight into records and arrays, with no JSON graph.
//! Members resolve against a static key table per record; undeclared members are skipped
//! by the reader. Dynamic `Json` subtrees use the graph path.
use super::*;

use perry_abi::{json::kind::*, layout::json_source};

impl<'s> FunctionBuilder<'s> {
    /// `JSON.parse(text)` where a record or array type is expected: decode it in one pass.
    /// None when the target has no generated decoder (the graph path applies).
    pub(super) fn parse_typed(
        &mut self,
        ctx: &mut Context,
        text: Value,
        target: TypeHandle,
    ) -> Result<Option<Value>> {
        let inner = optional_value(ctx, target).unwrap_or(target);
        let decodable = has_reader(ctx, inner)
            || inner.deref(ctx).is::<FP64Type>()
            || inner.deref(ctx).is::<StringType>()
            || inner.deref(ctx).is::<perry_mir::types::NumberStringType>()
            || self.is_bool_type(ctx, inner);
        if !decodable {
            return Ok(None);
        }
        let reader = self.intrinsic(ctx, Intrinsic::JsonReadBegin, vec![text]);
        let value = self.read_value(ctx, reader, target)?;
        self.intrinsic_void(ctx, Intrinsic::JsonReadEnd, vec![reader]);
        Ok(Some(value))
    }

    /// The next value of the reader as `ty`.
    fn read_value(&mut self, ctx: &mut Context, reader: Value, ty: TypeHandle) -> Result<Value> {
        if let Some((inner, states)) = optional_parts(ctx, ty) {
            if !states.admits_null() {
                // JSON has no `undefined`: a present member is the inner type.
                let value = self.read_value(ctx, reader, inner)?;
                return self.fit(ctx, value, ty);
            }
            let kind = self.intrinsic(ctx, Intrinsic::JsonReadKind, vec![reader]);
            let null = self.i32(ctx, u64::from(NULL));
            let is_null = self.compare(ctx, CompareKind::Eq, kind, null);
            return self.choose(
                ctx,
                ty,
                is_null,
                |b, ctx| {
                    b.intrinsic_void(ctx, Intrinsic::JsonReadSkip, vec![reader]);
                    Ok(b.emit_value(ctx, |ctx| NullOp::new(ctx, ty)))
                },
                |b, ctx| {
                    let value = b.read_value(ctx, reader, inner)?;
                    b.fit(ctx, value, ty)
                },
            );
        }
        let (number, string, number_string) = {
            let deref = ty.deref(ctx);
            (
                deref.is::<FP64Type>(),
                deref.is::<StringType>(),
                deref.is::<perry_mir::types::NumberStringType>(),
            )
        };
        if number {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonReadNumber, vec![reader]));
        }
        if string {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonReadString, vec![reader]));
        }
        if number_string {
            let kind = self.intrinsic(ctx, Intrinsic::JsonReadKind, vec![reader]);
            let number = self.i32(ctx, u64::from(NUMBER));
            let is_number = self.compare(ctx, CompareKind::Eq, kind, number);
            return self.choose(
                ctx,
                ty,
                is_number,
                |b, ctx| {
                    let value = b.intrinsic(ctx, Intrinsic::JsonReadNumber, vec![reader]);
                    b.fit(ctx, value, ty)
                },
                |b, ctx| {
                    let value = b.intrinsic(ctx, Intrinsic::JsonReadString, vec![reader]);
                    b.fit(ctx, value, ty)
                },
            );
        }
        if self.is_bool_type(ctx, ty) {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonReadBool, vec![reader]));
        }
        if has_reader(ctx, ty) {
            let name = self.reader_function(ctx, ty)?;
            let call = CallOp::new(ctx, name, vec![reader], vec![ty]);
            self.append(ctx, &call);
            return Ok(call.get_operation().deref(ctx).get_result(0));
        }
        // Dynamic members use their validated text through the graph path.
        let text = self.intrinsic(ctx, Intrinsic::JsonReadRaw, vec![reader]);
        let json = self.intrinsic(ctx, Intrinsic::JsonParse, vec![text]);
        self.fit(ctx, json, ty)
    }

    /// A variant from an object: the reader matches its tag against the case names, and a
    /// case's payload is read from `val` (resuming after the object once read).
    fn read_variant(
        &mut self,
        ctx: &mut Context,
        reader: Value,
        variant: TypedHandle<perry_mir::types::VariantType>,
    ) -> Result<Value> {
        let cases = variant.deref(ctx).cases().to_vec();
        let start = self.intrinsic(ctx, Intrinsic::JsonReadPosition, vec![reader]);
        let table = encode_keys(cases.iter().map(|(name, _)| name.as_str()));
        let table = self.string(ctx, &table);
        let prior = self.intrinsic(ctx, Intrinsic::JsonReadTakeError, vec![reader]);
        let tag = self.intrinsic(ctx, Intrinsic::JsonReadVariant, vec![reader, table]);
        let end = self.intrinsic(ctx, Intrinsic::JsonReadPosition, vec![reader]);
        let result = self.entry_slot(ctx, variant.into());
        let empty = self.emit_value(ctx, |ctx| UndefOp::new(ctx, variant.into()));
        self.store(ctx, result, empty);
        let done = self.new_block(ctx, "json_variant_done");
        for (index, (name, payload)) in cases.into_iter().enumerate() {
            let index = self.i32(ctx, index as u64);
            let matches = self.compare(ctx, CompareKind::Eq, tag, index);
            let active = self.new_block(ctx, "json_variant_case");
            let next = self.new_block(ctx, "json_variant_next");
            self.branch(ctx, matches, active, next);
            self.switch_to(active);
            let payload = if let Some(ty) = payload {
                let present = self.intrinsic(ctx, Intrinsic::JsonReadPayload, vec![reader]);
                Some(self.choose(
                    ctx,
                    ty,
                    present,
                    |b, ctx| {
                        let value = b.read_value(ctx, reader, ty)?;
                        b.intrinsic_void(ctx, Intrinsic::JsonReadResume, vec![reader, end]);
                        Ok(value)
                    },
                    |b, ctx| match missing_value(ctx, ty) {
                        Missing::Undefined => {
                            Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
                        }
                        Missing::Json => Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
                        Missing::Fails(kind) => {
                            let kind = b.i32(ctx, u64::from(kind));
                            b.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, kind]);
                            Ok(b.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)))
                        }
                    },
                )?)
            } else {
                None
            };
            let value = self.emit_value(ctx, |ctx| {
                perry_mir::variants::VariantNewOp::new(ctx, variant.into(), &name, payload)
            });
            let fields = if payload.is_some() {
                &["tag", "val"][..]
            } else {
                &["tag"][..]
            };
            let table = self.string(ctx, &encode_keys(fields.iter().copied()));
            let layout = self.i32(ctx, u64::from(json_source::VARIANT));
            self.intrinsic_void(
                ctx,
                Intrinsic::JsonReadAttach,
                vec![reader, value, start, table, layout],
            );
            self.store(ctx, result, value);
            self.jump(ctx, done);
            self.switch_to(next);
        }
        self.jump(ctx, done);
        self.switch_to(done);
        self.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, prior]);
        self.load(ctx, result)
    }

    /// The generated decoder of a record or array type, `(reader) -> ty`.
    fn reader_function(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Identifier> {
        if let Some(name) = self.shapes.readers.borrow().get(&ty) {
            return Ok(name.clone());
        }
        let mut index = self.shapes.readers.borrow().len();
        let name = loop {
            let name = identifier(&format!("perry_json_read_{index}"))?;
            if self.signatures.values().all(|s| s.name != name)
                && self
                    .shapes
                    .readers
                    .borrow()
                    .values()
                    .all(|other| *other != name)
            {
                break name;
            }
            index += 1;
        };
        // Registered before the body is built, so recursive types call themselves.
        self.shapes.readers.borrow_mut().insert(ty, name.clone());
        let reader_type = perry_mir::types::JsonReaderType::get(ctx).into();
        let function_type = FunctionType::get(ctx, vec![reader_type], vec![ty]);
        let function = FuncOp::new(ctx, name.clone(), function_type);
        self.shapes
            .module
            .append_operation(ctx, function.get_operation(), 0);
        let signature = Signature {
            name,
            is_async: false,
            params: vec![reader_type],
            results: vec![ty],
        };
        let mut builder = Self::new(
            ctx,
            &function,
            &signature,
            self.signatures,
            self.shapes,
            self.builtins,
        );
        let reader = builder.entry_block().deref(ctx).get_argument(0);
        let output = match TypedHandle::<perry_mir::types::VariantType>::from_handle(ty, ctx) {
            Ok(variant) => builder.read_variant(ctx, reader, variant)?,
            Err(_) => builder.read_aggregate(ctx, reader, ty)?,
        };
        builder.emit(ctx, |ctx| ReturnOp::new(ctx, vec![output]));
        remove_unreachable(ctx, builder.region);
        Ok(signature.name)
    }

    /// The body of a record, tuple or array decoder: the value with its source span attached,
    /// or a type error when the input is not an object or array.
    fn read_aggregate(
        &mut self,
        ctx: &mut Context,
        reader: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let start = self.intrinsic(ctx, Intrinsic::JsonReadPosition, vec![reader]);
        let prior = self.intrinsic(ctx, Intrinsic::JsonReadTakeError, vec![reader]);
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).ok();
        let tuple = record.is_some_and(|r| r.deref(ctx).is_tuple());
        let object = record.is_some() && !tuple;
        let table = record
            .and_then(|record| key_table(ctx, record))
            .unwrap_or_default();
        let table = self.string(ctx, &table);
        let opened = self.intrinsic(
            ctx,
            if object {
                Intrinsic::JsonReadObject
            } else {
                Intrinsic::JsonReadArray
            },
            vec![reader],
        );
        let output = self.choose(
            ctx,
            ty,
            opened,
            |b, ctx| match record {
                Some(record) if tuple => b.read_tuple(ctx, reader, record),
                Some(record) => b.read_record(ctx, reader, record, table),
                None => b.read_array(ctx, reader, ty),
            },
            |b, ctx| {
                let expected = b.i32(ctx, u64::from(if object { OBJECT } else { ARRAY }));
                b.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, expected]);
                Ok(b.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)))
            },
        )?;
        let layout = self.i32(
            ctx,
            u64::from(match record {
                Some(_) if tuple => json_source::TUPLE,
                Some(_) => json_source::RECORD,
                None => json_source::ARRAY,
            }),
        );
        self.intrinsic_void(
            ctx,
            Intrinsic::JsonReadAttach,
            vec![reader, output, start, table, layout],
        );
        self.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, prior]);
        Ok(output)
    }

    /// Initialize a decoded field and its deferred error. A present value replaces the
    /// missing-field error, so records need no separate presence flag.
    fn reader_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> (Value, Value) {
        let slot = self.entry_slot(ctx, ty);
        let kind = match missing_value(ctx, ty) {
            Missing::Undefined => {
                let undefined = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
                self.store(ctx, slot, undefined);
                0
            }
            Missing::Json => {
                let undefined = self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![]);
                self.store(ctx, slot, undefined);
                0
            }
            Missing::Fails(kind) => kind,
        };
        let index_type = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let error = self.entry_slot(ctx, index_type);
        let initial = self.i32(ctx, u64::from(kind));
        self.store(ctx, error, initial);
        (slot, error)
    }

    /// A record from an object: members dispatch on their index in the record's key table;
    /// the last duplicate wins. Required members that never came throw at the object's end.
    fn read_record(
        &mut self,
        ctx: &mut Context,
        reader: Value,
        record: TypedHandle<RecordType>,
        table: Value,
    ) -> Result<Value> {
        let fields = field_types(ctx, record)?;
        let index_type = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let slots: Vec<_> = fields.iter().map(|&ty| self.reader_slot(ctx, ty)).collect();
        let hint = self.entry_slot(ctx, index_type);
        let zero = self.i32(ctx, 0);
        self.store(ctx, hint, zero);
        let header = self.new_block(ctx, "json_member");
        let done = self.new_block(ctx, "json_members_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let expected = self.load(ctx, hint)?;
        let found = self.intrinsic(ctx, Intrinsic::JsonReadField, vec![reader, table, expected]);
        let end = self.i32(ctx, u64::from(u32::MAX));
        let ended = self.compare(ctx, CompareKind::Eq, found, end);
        let dispatch = self.new_block(ctx, "json_dispatch");
        self.branch(ctx, ended, done, dispatch);
        self.switch_to(dispatch);
        for (index, (&ty, &(slot, error))) in fields.iter().zip(&slots).enumerate() {
            let this = self.i32(ctx, index as u64);
            let matches = self.compare(ctx, CompareKind::Eq, found, this);
            let member = self.new_block(ctx, "json_field");
            let next = self.new_block(ctx, "json_field_next");
            self.branch(ctx, matches, member, next);
            self.switch_to(member);
            let value = self.read_value(ctx, reader, ty)?;
            self.store(ctx, slot, value);
            let mismatch = self.intrinsic(ctx, Intrinsic::JsonReadTakeError, vec![reader]);
            self.store(ctx, error, mismatch);
            let following = self.i32(ctx, index as u64 + 1);
            self.store(ctx, hint, following);
            self.jump(ctx, header);
            self.switch_to(next);
        }
        // The reader only returns indices of the table.
        self.jump(ctx, header);
        self.switch_to(done);
        let mut values = Vec::with_capacity(fields.len());
        for (slot, error) in slots {
            let mismatch = self.load(ctx, error)?;
            self.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, mismatch]);
            values.push(self.load(ctx, slot)?);
        }
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)))
    }

    /// A tuple from an array: elements by position, extra elements validated and ignored.
    fn read_tuple(
        &mut self,
        ctx: &mut Context,
        reader: Value,
        record: TypedHandle<RecordType>,
    ) -> Result<Value> {
        let fields = field_types(ctx, record)?;
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        // Whether the array has ended: `element` must not be asked again after that.
        let ended = self.entry_slot(ctx, boolean);
        let no = self.boolean(ctx, false);
        self.store(ctx, ended, no);
        let mut values = Vec::with_capacity(fields.len());
        for ty in fields {
            let (slot, error) = self.reader_slot(ctx, ty);
            let open = self.new_block(ctx, "json_tuple_open");
            let after = self.new_block(ctx, "json_tuple_after");
            let is_ended = self.load(ctx, ended)?;
            self.branch(ctx, is_ended, after, open);
            self.switch_to(open);
            let more = self.intrinsic(ctx, Intrinsic::JsonReadElement, vec![reader]);
            let element = self.new_block(ctx, "json_tuple_element");
            let closed = self.new_block(ctx, "json_tuple_closed");
            self.branch(ctx, more, element, closed);
            self.switch_to(closed);
            let yes = self.boolean(ctx, true);
            self.store(ctx, ended, yes);
            self.jump(ctx, after);
            self.switch_to(element);
            let value = self.read_value(ctx, reader, ty)?;
            self.store(ctx, slot, value);
            let zero = self.i32(ctx, 0);
            self.store(ctx, error, zero);
            self.jump(ctx, after);
            self.switch_to(after);
            values.push((slot, error));
        }
        // Skip elements beyond the tuple's.
        let rest = self.new_block(ctx, "json_tuple_rest");
        let extra = self.new_block(ctx, "json_tuple_extra");
        let done = self.new_block(ctx, "json_tuple_done");
        let is_ended = self.load(ctx, ended)?;
        self.branch(ctx, is_ended, done, rest);
        self.switch_to(rest);
        let more = self.intrinsic(ctx, Intrinsic::JsonReadElement, vec![reader]);
        self.branch(ctx, more, extra, done);
        self.switch_to(extra);
        self.intrinsic_void(ctx, Intrinsic::JsonReadSkip, vec![reader]);
        self.jump(ctx, rest);
        self.switch_to(done);
        let mut loaded = Vec::with_capacity(values.len());
        for (slot, error) in values {
            let missing = self.load(ctx, error)?;
            self.intrinsic_void(ctx, Intrinsic::JsonReadFail, vec![reader, missing]);
            loaded.push(self.load(ctx, slot)?);
        }
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, loaded)))
    }

    /// An array of decoded elements.
    fn read_array(&mut self, ctx: &mut Context, reader: Value, ty: TypeHandle) -> Result<Value> {
        let element = TypedHandle::<ArrayType>::from_handle(ty, ctx)
            .map_err(|_| anyhow!("expected an array type"))?
            .deref(ctx)
            .element();
        let array = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        let header = self.new_block(ctx, "json_element");
        let body = self.new_block(ctx, "json_element_body");
        let done = self.new_block(ctx, "json_elements_done");
        self.jump(ctx, header);
        self.switch_to(header);
        let more = self.intrinsic(ctx, Intrinsic::JsonReadElement, vec![reader]);
        self.branch(ctx, more, body, done);
        self.switch_to(body);
        let value = self.read_value(ctx, reader, element)?;
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, value));
        self.jump(ctx, header);
        self.switch_to(done);
        Ok(array)
    }
}

/// What a member that is absent from the object reads as.
enum Missing {
    /// `undefined`: the member is optional.
    Undefined,
    /// The JSON `undefined` of an untyped member.
    Json,
    /// A type error naming the expected kind.
    Fails(u32),
}

/// Whether `ty` is decoded by a generated reader function: a defined record (or tuple), an
/// array or a variant.
fn has_reader(ctx: &Context, ty: TypeHandle) -> bool {
    TypedHandle::<RecordType>::from_handle(ty, ctx)
        .is_ok_and(|record| record.deref(ctx).fields().is_ok())
        || ty.deref(ctx).is::<ArrayType>()
        || ty.deref(ctx).is::<perry_mir::types::VariantType>()
}

/// The field types of a defined record, in record order.
fn field_types(ctx: &Context, record: TypedHandle<RecordType>) -> Result<Vec<TypeHandle>> {
    Ok(record
        .deref(ctx)
        .fields()
        .map_err(|_| anyhow!("opaque JSON schema"))?
        .iter()
        .map(|&(_, ty)| ty)
        .collect())
}

fn missing_value(ctx: &Context, ty: TypeHandle) -> Missing {
    if absence(ctx, ty).is_some_and(|states| states.admits_undefined()) {
        return Missing::Undefined;
    }
    let inner = optional_value(ctx, ty).unwrap_or(ty);
    let deref = inner.deref(ctx);
    if deref.is::<JsonType>() {
        return Missing::Json;
    }
    Missing::Fails(if deref.is::<FP64Type>() {
        NUMBER
    } else if deref.is::<StringType>() || deref.is::<perry_mir::types::NumberStringType>() {
        STRING
    } else if deref.is::<ArrayType>()
        || deref
            .downcast_ref::<RecordType>()
            .is_some_and(|record| record.is_tuple())
    {
        ARRAY
    } else if deref
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 1)
    {
        BOOLEAN
    } else {
        OBJECT
    })
}

/// UTF-8-safe key table: five base-128 digits for each count, length and offset.
fn key_table(ctx: &Context, record: TypedHandle<RecordType>) -> Option<String> {
    let record = record.deref(ctx);
    if record.is_tuple() {
        return Some(String::new());
    }
    let fields = record.fields().ok()?;
    Some(encode_keys(fields.iter().map(|(name, _)| name.as_str())))
}

fn encode_keys<'a>(keys: impl ExactSizeIterator<Item = &'a str> + Clone) -> String {
    let mut table = String::with_capacity(perry_abi::json::key_table_len(keys.clone()));
    perry_abi::json::encode_keys(keys, |text| table.push_str(text));
    table
}
