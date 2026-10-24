//! MIR implementation of the frontend method registry.
use super::*;
use crate::array_methods::{Direction, FindResult, Mutation, Quantifier, Search, Transform};
use crate::collections::{SetOperation, SetRelation, kv_entry_types, set_entry_type};

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
impl<'s> CallExt<'s> for Call<'s> {
    fn lower(
        self,
        b: &mut FunctionBuilder<'s>,
        ctx: &mut Context,
        receiver: Value,
    ) -> Result<Option<Value>> {
        match self {
            Call::ArrayPush(args) => b.push_values(ctx, receiver, args).map(Some),
            Call::ArrayConcat(args) => b.concat_values(ctx, receiver, args).map(Some),
            Call::ArrayMap(callback) => b
                .array_transform(ctx, receiver, callback, Transform::Map)
                .map(Some),
            Call::ArrayFilter(callback) => b
                .array_transform(ctx, receiver, callback, Transform::Filter)
                .map(Some),
            Call::ArrayFlatMap(callback) => b
                .array_transform(ctx, receiver, callback, Transform::FlatMap)
                .map(Some),
            Call::ArrayFind(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Find(callback, FindResult::Value, Direction::Forward),
                )
                .map(Some),
            Call::ArrayFindIndex(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Find(callback, FindResult::Index, Direction::Forward),
                )
                .map(Some),
            Call::ArrayFindLast(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Find(callback, FindResult::Value, Direction::Reverse),
                )
                .map(Some),
            Call::ArrayFindLastIndex(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Find(callback, FindResult::Index, Direction::Reverse),
                )
                .map(Some),
            Call::ArraySome(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Quantifier(callback, Quantifier::Some),
                )
                .map(Some),
            Call::ArrayEvery(callback) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Quantifier(callback, Quantifier::Every),
                )
                .map(Some),
            Call::ArrayForEach(callback) => b.array_for_each(ctx, receiver, callback).map(Some),
            Call::ArraySort(comparator) => b
                .array_sort(ctx, receiver, comparator, Mutation::InPlace)
                .map(Some),
            Call::ArrayToSorted(comparator) => b
                .array_sort(ctx, receiver, comparator, Mutation::Copy)
                .map(Some),
            Call::ArrayReduce(callback, initial) => b
                .array_reduce(ctx, receiver, callback, initial, Direction::Forward)
                .map(Some),
            Call::ArrayReduceRight(callback, initial) => b
                .array_reduce(ctx, receiver, callback, initial, Direction::Reverse)
                .map(Some),
            Call::ArrayJoin(separator) => b.array_join(ctx, receiver, separator).map(Some),
            Call::ArraySlice(start, end) => b.array_slice(ctx, receiver, start, end).map(Some),
            Call::ArrayReverse() => b.array_reverse(ctx, receiver, Mutation::InPlace).map(Some),
            Call::ArrayToReversed() => b.array_reverse(ctx, receiver, Mutation::Copy).map(Some),
            Call::ArrayFlat(depth) => b.array_flat(ctx, receiver, depth).map(Some),
            Call::ArrayAt(index) => b.array_at(ctx, receiver, index).map(Some),
            Call::ArrayPop() => b.array_pop(ctx, receiver).map(Some),
            Call::ArrayWith(index, value) => b.array_with(ctx, receiver, index, value).map(Some),
            Call::ArrayIncludes(value, from) => b
                .array_search(ctx, receiver, Search::Includes(value, from))
                .map(Some),
            Call::ArrayIndexOf(value, from) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Index(value, from, Direction::Forward),
                )
                .map(Some),
            Call::ArrayLastIndexOf(value, from) => b
                .array_search(
                    ctx,
                    receiver,
                    Search::Index(value, from, Direction::Reverse),
                )
                .map(Some),
            Call::MapGet(key) => {
                let (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Map")?;
                let _ = value_type;

                let key = b.expression_with(ctx, key, Some(key_type))?;
                b.kv_get(ctx, receiver, key, key_type, value_type).map(Some)
            }
            Call::MapSet(key, value) => {
                let (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Map")?;
                let _ = value_type;

                let key = b.expression_with(ctx, key, Some(key_type))?;
                let value = b.expression_with(ctx, value, Some(value_type))?;
                b.kv_set(ctx, receiver, key, value).map(Some)
            }
            Call::MapHas(key) => {
                let (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Map")?;
                let _ = value_type;

                let key = b.expression_with(ctx, key, Some(key_type))?;
                b.kv_has(ctx, receiver, key, key_type).map(Some)
            }
            Call::MapDelete(key) => {
                let (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Map")?;
                let _ = value_type;

                let key = b.expression_with(ctx, key, Some(key_type))?;
                b.kv_delete(ctx, receiver, key, key_type).map(Some)
            }
            Call::MapClear() => b.collection_clear(ctx, receiver).map(Some),
            Call::MapForEach(callback) => b.collection_for_each(ctx, receiver, callback).map(Some),
            Call::SetAdd(value) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;

                let value = b.expression_with(ctx, value, Some(element))?;
                b.set_add(ctx, receiver, value, element).map(Some)
            }
            Call::SetHas(value) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;

                let value = b.expression_with(ctx, value, Some(element))?;
                b.kv_has(ctx, receiver, value, element).map(Some)
            }
            Call::SetDelete(value) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;

                let value = b.expression_with(ctx, value, Some(element))?;
                b.set_delete(ctx, receiver, value, element).map(Some)
            }
            Call::SetDifference(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_binary_op(ctx, receiver, other, element, SetOperation::Difference)
                    .map(Some)
            }
            Call::SetIntersection(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_binary_op(ctx, receiver, other, element, SetOperation::Intersection)
                    .map(Some)
            }
            Call::SetUnion(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_binary_op(ctx, receiver, other, element, SetOperation::Union)
                    .map(Some)
            }
            Call::SetSymmetricDifference(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_binary_op(
                    ctx,
                    receiver,
                    other,
                    element,
                    SetOperation::SymmetricDifference,
                )
                .map(Some)
            }
            Call::SetIsSubsetOf(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_relation_op(ctx, receiver, other, element, SetRelation::Subset)
                    .map(Some)
            }
            Call::SetIsSupersetOf(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_relation_op(ctx, receiver, other, element, SetRelation::Superset)
                    .map(Some)
            }
            Call::SetIsDisjointFrom(other) => {
                let (element, _) = set_entry_type(ctx, receiver.get_type(ctx))
                    .context("method receiver must be a Set")?;
                b.set_relation_op(ctx, receiver, other, element, SetRelation::Disjoint)
                    .map(Some)
            }
            Call::SetClear() => b.collection_clear(ctx, receiver).map(Some),
            Call::SetForEach(callback) => b.collection_for_each(ctx, receiver, callback).map(Some),
            Call::StringCharAt(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringCharAt,
                arguments: &[Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringCodePointAt(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringCodePointAt,
                arguments: &[Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringAt(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringAt,
                arguments: &[Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringPadStart(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringPadStart,
                arguments: &[Number(0.0), Padding],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringPadEnd(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringPadEnd,
                arguments: &[Number(0.0), Padding],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringSlice(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringSlice,
                arguments: &[Number(0.0), Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringSubstring(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringSubstring,
                arguments: &[Number(0.0), Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringIndexOf(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringIndexOf,
                arguments: &[SearchString, Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringLastIndexOf(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringLastIndexOf,
                arguments: &[SearchString, Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringIncludes(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringIndexOf,
                arguments: &[SearchString, Number(0.0)],
                result: Return::Found,
            }
            .lower(b, ctx, receiver, args),
            Call::StringStartsWith(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringStartsWith,
                arguments: &[SearchString, Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringEndsWith(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringEndsWith,
                arguments: &[SearchString, Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringTrim(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringTrim,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringTrimStart(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringTrimStart,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringTrimEnd(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringTrimEnd,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringToLowerCase(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringLower,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringToUpperCase(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringUpper,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringRepeat(args) => IntrinsicCall {
                intrinsic: Intrinsic::StringRepeat,
                arguments: &[Number(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::BytesSubarray(args) => IntrinsicCall {
                intrinsic: Intrinsic::BytesSubarray,
                arguments: &[Number(0.0), Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::BytesSlice(args) => IntrinsicCall {
                intrinsic: Intrinsic::BytesSlice,
                arguments: &[Number(0.0), Number(f64::INFINITY)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::BytesFill(args) => IntrinsicCall {
                intrinsic: Intrinsic::BytesFill,
                arguments: &[Number(0.0), Number(0.0), Number(f64::INFINITY)],
                result: Return::Receiver,
            }
            .lower(b, ctx, receiver, args),
            Call::BytesCryptoFillRandom(args) => IntrinsicCall {
                intrinsic: Intrinsic::BytesFillRandom,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::DateGetTime(args) => IntrinsicCall {
                intrinsic: Intrinsic::DateTime,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::DateToISOString(args) => IntrinsicCall {
                intrinsic: Intrinsic::DateIso,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::DateToJSON(args) => IntrinsicCall {
                intrinsic: Intrinsic::DateJson,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::DateGetFullYear(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(0.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetMonth(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(1.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetDate(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(2.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetDay(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(3.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetHours(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(4.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetMinutes(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(5.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetSeconds(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(6.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetMilliseconds(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(7.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateGetTimezoneOffset(_args) => IntrinsicCall {
                intrinsic: Intrinsic::DatePart,
                arguments: &[Constant(8.0)],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, []),
            Call::DateSetFullYear(args) => b.date_set(ctx, receiver, 0, 3, args),
            Call::DateSetMonth(args) => b.date_set(ctx, receiver, 1, 2, args),
            Call::DateSetDate(args) => b.date_set(ctx, receiver, 2, 1, args),
            Call::DateSetHours(args) => b.date_set(ctx, receiver, 3, 4, args),
            Call::DateSetMinutes(args) => b.date_set(ctx, receiver, 4, 3, args),
            Call::DateSetSeconds(args) => b.date_set(ctx, receiver, 5, 2, args),
            Call::DateSetMilliseconds(args) => b.date_set(ctx, receiver, 6, 1, args),
            Call::DateSetTime(args) => b.date_set(ctx, receiver, 7, 1, args),
            Call::StatsIsFile(args) => IntrinsicCall {
                intrinsic: Intrinsic::StatsIsFile,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StatsIsDirectory(args) => IntrinsicCall {
                intrinsic: Intrinsic::StatsIsDirectory,
                arguments: &[],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, args),
            Call::StringConcat(args) => {
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
            }
            Call::StringNormalize(form) => {
                let form = match form {
                    None | Some(Expr::Undefined) => b.string(ctx, "NFC"),
                    Some(arg) => b.string_operand(ctx, arg)?,
                };
                Ok(b.call_intrinsic(ctx, Intrinsic::StringNormalize, vec![receiver, form]))
            }
            Call::StringLocaleCompare(other) => {
                let other = b.string_operand(ctx, other)?;
                b.compare_sign(ctx, receiver, other).map(Some)
            }
            Call::StringSplit(separator) => {
                let sep_val = b.expression(ctx, separator)?;
                let sep_val = b.defined(ctx, sep_val)?;
                let intrinsic = match kind(ctx, sep_val)? {
                    Kind::String => Intrinsic::StringSplit,
                    Kind::Regex => Intrinsic::RegexSplit,
                    _ => bail!("string.split requires a string or literal regex separator"),
                };
                Ok(b.call_intrinsic(ctx, intrinsic, vec![receiver, sep_val]))
            }
            Call::StringReplace(search, replacement) => {
                b.replace_string(ctx, receiver, search, replacement, false)
            }
            Call::StringReplaceAll(search, replacement) => {
                b.replace_string(ctx, receiver, search, replacement, true)
            }
            Call::StringMatch(regex) => {
                let regex = b.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                Ok(b.call_intrinsic(ctx, Intrinsic::RegexMatch, vec![receiver, regex]))
            }
            Call::StringSearch(regex) => {
                let regex = b.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                ensure!(
                    !FunctionBuilder::regex_global(ctx, regex)?,
                    "Regex search supports only the u and s flags"
                );
                Ok(b.call_intrinsic(ctx, Intrinsic::RegexSearch, vec![receiver, regex]))
            }
            Call::StringCharCodeAt(_args) => {
                bail!("charCodeAt uses UTF-16 code units; use codePointAt with scalar indices")
            }
            Call::StringToString(_args) => Ok(Some(receiver)),
            Call::RegexTest(text) => {
                ensure!(
                    !FunctionBuilder::regex_global(ctx, receiver)?,
                    "Regex search supports only the u and s flags"
                );
                let text = b.string_operand(ctx, text)?;
                Ok(Some(b.regex_test(ctx, receiver, text)))
            }
            Call::RegexExec(text) => {
                let text = b.string_operand(ctx, text)?;
                Ok(b.call_intrinsic(ctx, Intrinsic::RegexExec, vec![text, receiver]))
            }
            Call::DecoderDecode(bytes, options) => {
                b.decode(ctx, receiver, bytes, options).map(Some)
            }
            Call::UrlString(_args) => b.url_field(ctx, receiver, "href").map(Some),
            Call::SearchParamsAppend(name, value) => {
                let name = b.string_operand(ctx, name)?;
                let value = b.string_operand(ctx, value)?;
                b.intrinsic_void(ctx, Intrinsic::UrlAppend, vec![receiver, name, value]);
                let ty = void_type(ctx);
                Ok(Some(
                    b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
                ))
            }
            Call::AbortAbort() => {
                b.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![receiver]);
                Ok(None)
            }
            Call::NumberToString(radix) => {
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
            }
            Call::BytesSet(args) => {
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
            }
            Call::CollectionKeys() => {
                ensure!(
                    crate::collections::dict_entry_types(ctx, receiver.get_type(ctx)).is_none(),
                    "dictionary values have no iterator method"
                );
                b.collection_iterator(ctx, receiver, crate::collections::IteratorMethod::Keys)
                    .map(Some)
            }
            Call::CollectionValues() => {
                ensure!(
                    crate::collections::dict_entry_types(ctx, receiver.get_type(ctx)).is_none(),
                    "dictionary values have no iterator method"
                );
                b.collection_iterator(ctx, receiver, crate::collections::IteratorMethod::Values)
                    .map(Some)
            }
            Call::CollectionEntries() => {
                ensure!(
                    crate::collections::dict_entry_types(ctx, receiver.get_type(ctx)).is_none(),
                    "dictionary values have no iterator method"
                );
                b.collection_iterator(ctx, receiver, crate::collections::IteratorMethod::Entries)
                    .map(Some)
            }
            Call::InstantToString() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::InstantString, vec![receiver]))
            }
            Call::PlainDateToString() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::PlainDateString, vec![receiver]))
            }
            Call::PlainDateTimeToString() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::PlainDateTimeString, vec![receiver]))
            }
            Call::PlainDateEquals(other) => {
                let other = b.coerce_plain_date(ctx, other, None)?;
                let left = b.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![receiver]);
                let right = b.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![other]);
                Ok(Some(b.compare(ctx, CompareKind::Eq, left, right)))
            }
            Call::PlainDateUntil(other, options) => {
                let other = b.coerce_plain_date(ctx, other, None)?;
                b.temporal_until(ctx, receiver, other, options).map(Some)
            }
            Call::PlainDateSince(other, options) => {
                let other = b.coerce_plain_date(ctx, other, None)?;
                b.temporal_until(ctx, other, receiver, options).map(Some)
            }
            Call::TemporalAdd(duration, options) => {
                b.temporal_add(ctx, receiver, duration, options, false)
            }
            Call::TemporalSubtract(duration, options) => {
                b.temporal_add(ctx, receiver, duration, options, true)
            }
            Call::HeadersAppend(name, value) => IntrinsicCall {
                intrinsic: Intrinsic::HeadersAppend,
                arguments: &[
                    Required(Kind::String, "a string"),
                    Required(Kind::String, "a string"),
                ],
                result: Return::Undefined,
            }
            .lower(b, ctx, receiver, [name, value]),
            Call::HeadersSet(name, value) => IntrinsicCall {
                intrinsic: Intrinsic::HeadersSet,
                arguments: &[
                    Required(Kind::String, "a string"),
                    Required(Kind::String, "a string"),
                ],
                result: Return::Undefined,
            }
            .lower(b, ctx, receiver, [name, value]),
            Call::HeadersDelete(name) => IntrinsicCall {
                intrinsic: Intrinsic::HeadersDelete,
                arguments: &[Required(Kind::String, "a string")],
                result: Return::Undefined,
            }
            .lower(b, ctx, receiver, [name]),
            Call::HeadersGet(name) => IntrinsicCall {
                intrinsic: Intrinsic::HeadersGet,
                arguments: &[Required(Kind::String, "a string")],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, [name]),
            Call::HeadersHas(name) => IntrinsicCall {
                intrinsic: Intrinsic::HeadersHas,
                arguments: &[Required(Kind::String, "a string")],
                result: Return::Value,
            }
            .lower(b, ctx, receiver, [name]),
            Call::ResponseStatus() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseStatus, vec![receiver]))
            }
            Call::ResponseStatusText() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseStatusText, vec![receiver]))
            }
            Call::ResponseOk() => Ok(b.call_intrinsic(ctx, Intrinsic::ResponseOk, vec![receiver])),
            Call::ResponseHeaders() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseHeaders, vec![receiver]))
            }
            Call::ResponseBody() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseBody, vec![receiver]))
            }
            Call::ResponseBodyUsed() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseBodyUsed, vec![receiver]))
            }
            Call::ResponseBytes() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseBytes, vec![receiver]))
            }
            Call::ResponseText() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseText, vec![receiver]))
            }
            Call::ResponseJson() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ResponseJson, vec![receiver]))
            }
            Call::RequestVerb() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestMethod, vec![receiver]))
            }
            Call::RequestUrl() => Ok(b.call_intrinsic(ctx, Intrinsic::RequestUrl, vec![receiver])),
            Call::RequestHeaders() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestHeaders, vec![receiver]))
            }
            Call::RequestBody() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestBody, vec![receiver]))
            }
            Call::RequestBodyUsed() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestBodyUsed, vec![receiver]))
            }
            Call::RequestBytes() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestBytes, vec![receiver]))
            }
            Call::RequestText() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestText, vec![receiver]))
            }
            Call::RequestJson() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::RequestJson, vec![receiver]))
            }
            Call::WritableLocked() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::WritableLocked, vec![receiver]))
            }
            Call::WritableGetWriter() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::WritableGetWriter, vec![receiver]))
            }
            Call::WriterWrite(chunk) => {
                let (chunk, _) = b.body_init(ctx, Some(chunk))?;
                let chunk = b.defined(ctx, chunk)?;
                Ok(b.call_intrinsic(ctx, Intrinsic::WriterWrite, vec![receiver, chunk]))
            }
            Call::WriterClose() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::WriterClose, vec![receiver]))
            }
            Call::WriterReleaseLock() => IntrinsicCall {
                intrinsic: Intrinsic::WriterReleaseLock,
                arguments: &[],
                result: Return::Undefined,
            }
            .lower(b, ctx, receiver, []),
            Call::ReadableLocked() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ReadableLocked, vec![receiver]))
            }
            Call::ReadableGetReader(options) => {
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
            }
            Call::ReadableCancel() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ReadableCancel, vec![receiver]))
            }
            Call::ReaderRead() => Ok(b.call_intrinsic(ctx, Intrinsic::ReaderRead, vec![receiver])),
            Call::ReaderReadByob(view, options) => {
                let view = b.typed_operand(ctx, view, Kind::Bytes, "a Uint8Array")?;
                let fields = b.init_fields(options)?;
                ensure!(
                    fields.is_empty() || (fields.len() == 1 && fields[0].0 == "min"),
                    "BYOB read supports only {{min}}"
                );
                let minimum = b.number_or(ctx, fields.first().map(|(_, value)| *value), 1.0)?;
                Ok(b.call_intrinsic(
                    ctx,
                    Intrinsic::ReaderReadByob,
                    vec![receiver, view, minimum],
                ))
            }
            Call::ReaderCancel() => {
                Ok(b.call_intrinsic(ctx, Intrinsic::ReaderCancel, vec![receiver]))
            }
            Call::ReaderReleaseLock() => IntrinsicCall {
                intrinsic: Intrinsic::ReaderReleaseLock,
                arguments: &[],
                result: Return::Undefined,
            }
            .lower(b, ctx, receiver, []),
            Call::ReadDone() => Ok(b.call_intrinsic(ctx, Intrinsic::ReadDone, vec![receiver])),
            Call::ReadValue() => Ok(b.call_intrinsic(ctx, Intrinsic::ReadValue, vec![receiver])),
        }
    }
}
