//! MIR implementation of the frontend method registry.
use super::*;
use crate::array_methods::{Direction, FindResult, Mutation, Quantifier, Search, Transform};
use crate::collections::{
    IteratorMethod, SetOperation, SetRelation, dict_entry_types, kv_entry_types, set_entry_type,
};

mod intrinsic;
use intrinsic::{
    Argument::{Constant, Number, Padding, Required, SearchString},
    IntrinsicCall, Return,
};

pub(super) use perry_frontend::methods::{Call, CallReceiver, Receiver};

pub(super) trait ReceiverExt {
    fn of(ctx: &Context, value: Value) -> Result<Receiver>;
}
impl ReceiverExt for Receiver {
    fn of(ctx: &Context, value: Value) -> Result<Self> {
        let ty = value.get_type(ctx);
        Ok(if crate::collections::map_entry_types(ctx, ty).is_some() {
            Self::Map
        } else if set_entry_type(ctx, ty).is_some() {
            Self::Set
        } else {
            Self::Value(kind(ctx, value)?)
        })
    }
}
pub(super) trait CallExt<'s> {
    fn lower(
        self,
        builder: &mut FunctionBuilder<'s>,
        ctx: &mut Context,
        receiver: Value,
    ) -> Result<Option<Value>>;
}

/// Lowers one `Call` variant per row. Each group's setup block runs before its rows' actions.
/// The generated `match` has no wildcard, so a registry method without a row fails to compile.
macro_rules! lower_calls {
    ($call:expr, $b:ident, $ctx:ident, $receiver:ident;
        $( $setup:tt { $( $variant:ident($($arg:pat),*) => $action:ident $lower:tt; )* } )*
    ) => {
        match $call { $($( Call::$variant($($arg),*) => {
            lower_calls!(@setup $setup);
            lower_calls!(@invoke ($b, $ctx, $receiver) $action $lower)
        } )*)* }
    };
    (@setup { $($setup:tt)* }) => { $($setup)* };
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) code $lower:block) => { $lower };
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) value ($method:ident($($arg:expr),*))) => {
        $b.$method($ctx, $receiver $(, $arg)*).map(Some)
    };
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) optional ($method:ident($($arg:expr),*))) => {
        $b.$method($ctx, $receiver $(, $arg)*)
    };
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) intrinsic ($intrinsic:ident)) => {
        Ok($b.call_intrinsic($ctx, Intrinsic::$intrinsic, vec![$receiver]))
    };
    (@invoke ($b:ident, $ctx:ident, $receiver:ident)
        intrinsic ($intrinsic:ident, $args:expr, [$($argument:expr),*], $result:ident)) => {
        IntrinsicCall { intrinsic: Intrinsic::$intrinsic, arguments: &[$($argument),*], result: Return::$result }
            .lower($b, $ctx, $receiver, $args)
    };
}

impl<'s> CallExt<'s> for Call<'s> {
    fn lower(
        self,
        b: &mut FunctionBuilder<'s>,
        ctx: &mut Context,
        receiver: Value,
    ) -> Result<Option<Value>> {
        lower_calls! { self, b, ctx, receiver;
            {} {
                ArrayPush(args) => value(push_values(args));
                ArrayConcat(args) => value(concat_values(args));
                ArrayMap(callback) => value(array_transform(callback, Transform::Map));
                ArrayFilter(callback) => value(array_transform(callback, Transform::Filter));
                ArrayFlatMap(callback) => value(array_transform(callback, Transform::FlatMap));
                ArrayFind(callback) => value(array_search(Search::Find(callback, FindResult::Value, Direction::Forward)));
                ArrayFindIndex(callback) => value(array_search(Search::Find(callback, FindResult::Index, Direction::Forward)));
                ArrayFindLast(callback) => value(array_search(Search::Find(callback, FindResult::Value, Direction::Reverse)));
                ArrayFindLastIndex(callback) => value(array_search(Search::Find(callback, FindResult::Index, Direction::Reverse)));
                ArraySome(callback) => value(array_search(Search::Quantifier(callback, Quantifier::Some)));
                ArrayEvery(callback) => value(array_search(Search::Quantifier(callback, Quantifier::Every)));
                ArrayForEach(callback) => value(array_for_each(callback));
                ArraySort(comparator) => value(array_sort(comparator, Mutation::InPlace));
                ArrayToSorted(comparator) => value(array_sort(comparator, Mutation::Copy));
                ArrayReduce(callback, initial) => value(array_reduce(callback, initial, Direction::Forward));
                ArrayReduceRight(callback, initial) => value(array_reduce(callback, initial, Direction::Reverse));
                ArrayJoin(separator) => value(array_join(separator));
                ArraySlice(start, end) => value(array_slice(start, end));
                ArrayReverse() => value(array_reverse(Mutation::InPlace));
                ArrayToReversed() => value(array_reverse(Mutation::Copy));
                ArrayFlat(depth) => value(array_flat(depth));
                ArrayAt(index) => value(array_at(index));
                ArrayPop() => value(array_pop());
                ArrayWith(index, value) => value(array_with(index, value));
                ArrayIncludes(value, from) => value(array_search(Search::Includes(value, from)));
                ArrayIndexOf(value, from) => value(array_search(Search::Index(value, from, Direction::Forward)));
                ArrayLastIndexOf(value, from) => value(array_search(Search::Index(value, from, Direction::Reverse)));
                MapClear() => value(collection_clear());
                MapForEach(callback) => value(collection_for_each(callback));
                SetClear() => value(collection_clear());
                SetForEach(callback) => value(collection_for_each(callback));
            }
            {
                let (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Map")?;
                let _ = value_type;
            } {
                MapGet(key) => code {
                    let key = b.expression_with(ctx, key, Some(key_type))?;
                    b.kv_get(ctx, receiver, key, key_type, value_type).map(Some)
                };
                MapSet(key, value) => code {
                    let key = b.expression_with(ctx, key, Some(key_type))?;
                    let value = b.expression_with(ctx, value, Some(value_type))?;
                    b.kv_set(ctx, receiver, key, value).map(Some)
                };
                MapHas(key) => code {
                    let key = b.expression_with(ctx, key, Some(key_type))?;
                    b.kv_has(ctx, receiver, key, key_type).map(Some)
                };
                MapDelete(key) => code {
                    let key = b.expression_with(ctx, key, Some(key_type))?;
                    b.kv_delete(ctx, receiver, key, key_type).map(Some)
                };
            }
            {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
            } {
                SetAdd(value) => code {
                    let value = b.expression_with(ctx, value, Some(element))?;
                    b.set_add(ctx, receiver, value, element).map(Some)
                };
                SetHas(value) => code {
                    let value = b.expression_with(ctx, value, Some(element))?;
                    b.kv_has(ctx, receiver, value, element).map(Some)
                };
                SetDelete(value) => code {
                    let value = b.expression_with(ctx, value, Some(element))?;
                    b.set_delete(ctx, receiver, value, element).map(Some)
                };
                SetDifference(other) => value(set_binary_op(other, element, SetOperation::Difference));
                SetIntersection(other) => value(set_binary_op(other, element, SetOperation::Intersection));
                SetUnion(other) => value(set_binary_op(other, element, SetOperation::Union));
                SetSymmetricDifference(other) => value(set_binary_op(other, element, SetOperation::SymmetricDifference));
                SetIsSubsetOf(other) => value(set_relation_op(other, element, SetRelation::Subset));
                SetIsSupersetOf(other) => value(set_relation_op(other, element, SetRelation::Superset));
                SetIsDisjointFrom(other) => value(set_relation_op(other, element, SetRelation::Disjoint));
            }
            {
                ensure!(
                    dict_entry_types(ctx, receiver.get_type(ctx)).is_none(),
                    "dictionary values have no iterator method"
                );
            } {
                CollectionKeys() => value(collection_iterator(IteratorMethod::Keys));
                CollectionValues() => value(collection_iterator(IteratorMethod::Values));
                CollectionEntries() => value(collection_iterator(IteratorMethod::Entries));
            }
            {} {
                StringCharAt(args) => intrinsic(StringCharAt, args, [Number(0.0)], Value);
                StringCodePointAt(args) => intrinsic(StringCodePointAt, args, [Number(0.0)], Value);
                StringAt(args) => intrinsic(StringAt, args, [Number(0.0)], Value);
                StringPadStart(args) => intrinsic(StringPadStart, args, [Number(0.0), Padding], Value);
                StringPadEnd(args) => intrinsic(StringPadEnd, args, [Number(0.0), Padding], Value);
                StringSlice(args) => intrinsic(StringSlice, args, [Number(0.0), Number(f64::INFINITY)], Value);
                StringSubstring(args) => intrinsic(StringSubstring, args, [Number(0.0), Number(f64::INFINITY)], Value);
                StringIndexOf(args) => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Value);
                StringLastIndexOf(args) => intrinsic(StringLastIndexOf, args, [SearchString, Number(f64::INFINITY)], Value);
                StringIncludes(args) => intrinsic(StringIndexOf, args, [SearchString, Number(0.0)], Found);
                StringStartsWith(args) => intrinsic(StringStartsWith, args, [SearchString, Number(0.0)], Value);
                StringEndsWith(args) => intrinsic(StringEndsWith, args, [SearchString, Number(f64::INFINITY)], Value);
                StringTrim(args) => intrinsic(StringTrim, args, [], Value);
                StringTrimStart(args) => intrinsic(StringTrimStart, args, [], Value);
                StringTrimEnd(args) => intrinsic(StringTrimEnd, args, [], Value);
                StringToLowerCase(args) => intrinsic(StringLower, args, [], Value);
                StringToUpperCase(args) => intrinsic(StringUpper, args, [], Value);
                StringRepeat(args) => intrinsic(StringRepeat, args, [Number(0.0)], Value);
                BytesSubarray(args) => intrinsic(BytesSubarray, args, [Number(0.0), Number(f64::INFINITY)], Value);
                BytesSlice(args) => intrinsic(BytesSlice, args, [Number(0.0), Number(f64::INFINITY)], Value);
                BytesFill(args) => intrinsic(BytesFill, args, [Number(0.0), Number(0.0), Number(f64::INFINITY)], Receiver);
                BytesCryptoFillRandom(args) => intrinsic(BytesFillRandom, args, [], Value);
                DateGetTime(args) => intrinsic(DateTime, args, [], Value);
                DateToISOString(args) => intrinsic(DateIso, args, [], Value);
                DateToJSON(args) => intrinsic(DateJson, args, [], Value);
                DateGetFullYear(_) => intrinsic(DatePart, [], [Constant(0.0)], Value);
                DateGetMonth(_) => intrinsic(DatePart, [], [Constant(1.0)], Value);
                DateGetDate(_) => intrinsic(DatePart, [], [Constant(2.0)], Value);
                DateGetDay(_) => intrinsic(DatePart, [], [Constant(3.0)], Value);
                DateGetHours(_) => intrinsic(DatePart, [], [Constant(4.0)], Value);
                DateGetMinutes(_) => intrinsic(DatePart, [], [Constant(5.0)], Value);
                DateGetSeconds(_) => intrinsic(DatePart, [], [Constant(6.0)], Value);
                DateGetMilliseconds(_) => intrinsic(DatePart, [], [Constant(7.0)], Value);
                DateGetTimezoneOffset(_) => intrinsic(DatePart, [], [Constant(8.0)], Value);
                DateSetFullYear(args) => optional(date_set(0, 3, args));
                DateSetMonth(args) => optional(date_set(1, 2, args));
                DateSetDate(args) => optional(date_set(2, 1, args));
                DateSetHours(args) => optional(date_set(3, 4, args));
                DateSetMinutes(args) => optional(date_set(4, 3, args));
                DateSetSeconds(args) => optional(date_set(5, 2, args));
                DateSetMilliseconds(args) => optional(date_set(6, 1, args));
                DateSetTime(args) => optional(date_set(7, 1, args));
                StatsIsFile(args) => intrinsic(StatsIsFile, args, [], Value);
                StatsIsDirectory(args) => intrinsic(StatsIsDirectory, args, [], Value);
                StringConcat(args) => code {
                    let mut segments = Vec::with_capacity(args.len());
                    for argument in args {
                        let value = b.expression(ctx, argument)?;
                        segments.push(b.display(ctx, value)?);
                    }
                    let mut result = receiver;
                    for segment in segments {
                        result = b.emit_value(ctx, |ctx| ConcatOp::new(ctx, result, segment));
                    }
                    Ok(Some(result))
                };
                StringNormalize(form) => code {
                    let form = match form {
                        None | Some(Expr::Undefined) => b.string(ctx, "NFC"),
                        Some(arg) => b.string_operand(ctx, arg)?,
                    };
                    Ok(b.call_intrinsic(ctx, Intrinsic::StringNormalize, vec![receiver, form]))
                };
                StringLocaleCompare(other) => code {
                    let other = b.string_operand(ctx, other)?;
                    b.compare_sign(ctx, receiver, other).map(Some)
                };
                StringSplit(separator) => code {
                    let sep_val = b.expression(ctx, separator)?;
                    let sep_val = b.defined(ctx, sep_val)?;
                    let intrinsic = match kind(ctx, sep_val)? {
                        Kind::String => Intrinsic::StringSplit,
                        Kind::Regex => Intrinsic::RegexSplit,
                        _ => bail!("string.split requires a string or literal regex separator"),
                    };
                    Ok(b.call_intrinsic(ctx, intrinsic, vec![receiver, sep_val]))
                };
                StringReplace(search, replacement) => optional(replace_string(search, replacement, false));
                StringReplaceAll(search, replacement) => optional(replace_string(search, replacement, true));
                StringMatch(regex) => code {
                    let regex = b.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                    Ok(b.call_intrinsic(ctx, Intrinsic::RegexMatch, vec![receiver, regex]))
                };
                StringSearch(regex) => code {
                    let regex = b.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                    ensure!(
                        !FunctionBuilder::regex_global(ctx, regex)?,
                        "Regex search supports only the u and s flags"
                    );
                    Ok(b.call_intrinsic(ctx, Intrinsic::RegexSearch, vec![receiver, regex]))
                };
                StringCharCodeAt(_) => code {
                    bail!("charCodeAt uses UTF-16 code units; use codePointAt with scalar indices")
                };
                StringToString(_) => code { Ok(Some(receiver)) };
                RegexTest(text) => code {
                    ensure!(
                        !FunctionBuilder::regex_global(ctx, receiver)?,
                        "Regex search supports only the u and s flags"
                    );
                    let text = b.string_operand(ctx, text)?;
                    Ok(Some(b.regex_test(ctx, receiver, text)))
                };
                RegexExec(text) => code {
                    let text = b.string_operand(ctx, text)?;
                    Ok(b.call_intrinsic(ctx, Intrinsic::RegexExec, vec![text, receiver]))
                };
                DecoderDecode(bytes, options) => value(decode(bytes, options));
                UrlString(_) => value(url_field("href"));
                SearchParamsAppend(name, value) => code {
                    let name = b.string_operand(ctx, name)?;
                    let value = b.string_operand(ctx, value)?;
                    b.intrinsic_void(ctx, Intrinsic::UrlAppend, vec![receiver, name, value]);
                    let ty = void_type(ctx);
                    Ok(Some(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))))
                };
                AbortAbort() => code {
                    b.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![receiver]);
                    Ok(None)
                };
                NumberToString(radix) => code {
                    let num = b.arithmetic(ctx, receiver);
                    Ok(match radix {
                        None | Some(Expr::Undefined) => {
                            b.call_intrinsic(ctx, Intrinsic::NumberToString, vec![num])
                        }
                        Some(radix_expr) => {
                            let radix = b.number_operand(ctx, radix_expr)?;
                            b.call_intrinsic(ctx, Intrinsic::NumberToStringRadix, vec![num, radix])
                        }
                    })
                };
                BytesSet(args) => code {
                    let source = args.first().context("set needs a source")?;
                    let source = b.expression(ctx, source)?;
                    let source = b.defined(ctx, source)?;
                    let source = match kind(ctx, source)? {
                        Kind::Bytes => source,
                        Kind::Array => b.intrinsic(ctx, Intrinsic::BytesFromArray, vec![source]),
                        _ => bail!("Uint8Array.set needs a Uint8Array or number[] source"),
                    };
                    let offset = b.number_or(ctx, args.get(1), 0.0)?;
                    b.intrinsic_void(ctx, Intrinsic::BytesSetFrom, vec![receiver, source, offset]);
                    Ok(None)
                };
                InstantToString() => intrinsic(InstantString);
                PlainDateToString() => intrinsic(PlainDateString);
                PlainDateTimeToString() => intrinsic(PlainDateTimeString);
                PlainDateEquals(other) => code {
                    let other = b.coerce_plain_date(ctx, other, None)?;
                    let left = b.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![receiver]);
                    let right = b.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![other]);
                    Ok(Some(b.compare(ctx, CompareKind::Eq, left, right)))
                };
                PlainDateUntil(other, options) => code {
                    let other = b.coerce_plain_date(ctx, other, None)?;
                    b.temporal_until(ctx, receiver, other, options).map(Some)
                };
                PlainDateSince(other, options) => code {
                    let other = b.coerce_plain_date(ctx, other, None)?;
                    b.temporal_until(ctx, other, receiver, options).map(Some)
                };
                TemporalAdd(duration, options) => optional(temporal_add(duration, options, false));
                TemporalSubtract(duration, options) => optional(temporal_add(duration, options, true));
                HeadersAppend(name, value) => intrinsic(HeadersAppend, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined);
                HeadersSet(name, value) => intrinsic(HeadersSet, [name, value], [Required(Kind::String, "a string"), Required(Kind::String, "a string")], Undefined);
                HeadersDelete(name) => intrinsic(HeadersDelete, [name], [Required(Kind::String, "a string")], Undefined);
                HeadersGet(name) => intrinsic(HeadersGet, [name], [Required(Kind::String, "a string")], Value);
                HeadersHas(name) => intrinsic(HeadersHas, [name], [Required(Kind::String, "a string")], Value);
                ResponseStatus() => intrinsic(ResponseStatus);
                ResponseStatusText() => intrinsic(ResponseStatusText);
                ResponseOk() => intrinsic(ResponseOk);
                ResponseHeaders() => intrinsic(ResponseHeaders);
                ResponseBody() => intrinsic(ResponseBody);
                ResponseBodyUsed() => intrinsic(ResponseBodyUsed);
                ResponseBytes() => intrinsic(ResponseBytes);
                ResponseText() => intrinsic(ResponseText);
                ResponseJson() => intrinsic(ResponseJson);
                RequestVerb() => intrinsic(RequestMethod);
                RequestUrl() => intrinsic(RequestUrl);
                RequestHeaders() => intrinsic(RequestHeaders);
                RequestBody() => intrinsic(RequestBody);
                RequestBodyUsed() => intrinsic(RequestBodyUsed);
                RequestBytes() => intrinsic(RequestBytes);
                RequestText() => intrinsic(RequestText);
                RequestJson() => intrinsic(RequestJson);
                WritableLocked() => intrinsic(WritableLocked);
                WritableGetWriter() => intrinsic(WritableGetWriter);
                WriterWrite(chunk) => code {
                    let (chunk, _) = b.body_init(ctx, Some(chunk))?;
                    let chunk = b.defined(ctx, chunk)?;
                    Ok(b.call_intrinsic(ctx, Intrinsic::WriterWrite, vec![receiver, chunk]))
                };
                WriterClose() => intrinsic(WriterClose);
                WriterReleaseLock() => intrinsic(WriterReleaseLock, [], [], Undefined);
                ReadableLocked() => intrinsic(ReadableLocked);
                ReadableGetReader(options) => code {
                    let intrinsic = if let Some(options) = options {
                        let fields = b.init_fields(Some(options))?;
                        ensure!(
                            fields.len() == 1
                                && fields[0].0 == "mode"
                                && matches!(fields[0].1, Expr::String(mode) if mode == "byob"),
                            "getReader supports only {{mode: 'byob'}}"
                        );
                        Intrinsic::ReadableGetByobReader
                    } else {
                        Intrinsic::ReadableGetReader
                    };
                    Ok(b.call_intrinsic(ctx, intrinsic, vec![receiver]))
                };
                ReadableCancel() => intrinsic(ReadableCancel);
                ReaderRead() => intrinsic(ReaderRead);
                ReaderReadByob(view, options) => code {
                    let view = b.typed_operand(ctx, view, Kind::Bytes, "a Uint8Array")?;
                    let fields = b.init_fields(options)?;
                    ensure!(
                        fields.is_empty() || (fields.len() == 1 && fields[0].0 == "min"),
                        "BYOB read supports only {{min}}"
                    );
                    let minimum = b.number_or(ctx, fields.first().map(|(_, value)| *value), 1.0)?;
                    Ok(b.call_intrinsic(ctx, Intrinsic::ReaderReadByob, vec![receiver, view, minimum]))
                };
                ReaderCancel() => intrinsic(ReaderCancel);
                ReaderReleaseLock() => intrinsic(ReaderReleaseLock, [], [], Undefined);
                ReadDone() => intrinsic(ReadDone);
                ReadValue() => intrinsic(ReadValue);
            }
        }
    }
}
