//! MIR lowering of the frontend method catalog.
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

/// Implements `Call::lower` from the lowering column of `perry_frontend::method_catalog`.
/// Every catalog row carries a recipe, so each `Call` variant has exactly one arm.
macro_rules! lower_calls {
    ($( $family:pat, $class:literal {
        $( $variant:ident $($name:literal)|+ $result:ident $(($result_arg:expr))? $preserve:ident $form:ident
            [$($argument:ident $(:$mode:ident)?),*] => $action:ident $recipe:tt, $requirement:literal;
        )*
    } )*) => {
        impl<'s> CallExt<'s> for Call<'s> {
            fn lower(
                self, b: &mut FunctionBuilder<'s>, ctx: &mut Context, receiver: Value,
            ) -> Result<Option<Value>> {
                match self { $($(
                    Call::$variant($($argument),*) => lower_calls!(@invoke (b, ctx, receiver) $action $recipe),
                )*)* }
            }
        }
    };
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
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) receiver ($args:expr)) => {{
        for arg in $args {
            $b.expression($ctx, arg)?;
        }
        Ok(Some($receiver))
    }};
}

perry_frontend::method_catalog!(lower_calls);

fn map_types(ctx: &Context, map: Value) -> Result<(TypeHandle, TypeHandle)> {
    let (key, value, _) =
        kv_entry_types(ctx, map.get_type(ctx)).context("method receiver must be a Map")?;
    Ok((key, value))
}

fn set_element(ctx: &Context, set: Value) -> Result<TypeHandle> {
    let (element, _) =
        set_entry_type(ctx, set.get_type(ctx)).context("method receiver must be a Set")?;
    Ok(element)
}

/// Catalog recipes that combine several builder operations.
impl<'s> FunctionBuilder<'s> {
    fn iterator_method(
        &mut self,
        ctx: &mut Context,
        collection: Value,
        method: IteratorMethod,
    ) -> Result<Value> {
        ensure!(
            dict_entry_types(ctx, collection.get_type(ctx)).is_none(),
            "dictionary values have no iterator method"
        );
        self.collection_iterator(ctx, collection, method)
    }

    fn map_get(&mut self, ctx: &mut Context, map: Value, key: &'s Expr) -> Result<Value> {
        let (key_type, value_type) = map_types(ctx, map)?;
        let key = self.expression_with(ctx, key, Some(key_type))?;
        self.kv_get(ctx, map, key, key_type, value_type)
    }

    fn map_set(
        &mut self,
        ctx: &mut Context,
        map: Value,
        key: &'s Expr,
        value: &'s Expr,
    ) -> Result<Value> {
        let (key_type, value_type) = map_types(ctx, map)?;
        let key = self.expression_with(ctx, key, Some(key_type))?;
        let value = self.expression_with(ctx, value, Some(value_type))?;
        self.kv_set(ctx, map, key, value)
    }

    fn map_has(&mut self, ctx: &mut Context, map: Value, key: &'s Expr) -> Result<Value> {
        let (key_type, _) = map_types(ctx, map)?;
        let key = self.expression_with(ctx, key, Some(key_type))?;
        self.kv_has(ctx, map, key, key_type)
    }

    fn map_delete(&mut self, ctx: &mut Context, map: Value, key: &'s Expr) -> Result<Value> {
        let (key_type, _) = map_types(ctx, map)?;
        let key = self.expression_with(ctx, key, Some(key_type))?;
        self.kv_delete(ctx, map, key, key_type)
    }

    fn set_insert(&mut self, ctx: &mut Context, set: Value, value: &'s Expr) -> Result<Value> {
        let element = set_element(ctx, set)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        self.set_add(ctx, set, value, element)
    }

    fn set_contains(&mut self, ctx: &mut Context, set: Value, value: &'s Expr) -> Result<Value> {
        let element = set_element(ctx, set)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        self.kv_has(ctx, set, value, element)
    }

    fn set_remove(&mut self, ctx: &mut Context, set: Value, value: &'s Expr) -> Result<Value> {
        let element = set_element(ctx, set)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        self.set_delete(ctx, set, value, element)
    }

    fn set_operation(
        &mut self,
        ctx: &mut Context,
        set: Value,
        other: &'s Expr,
        operation: SetOperation,
    ) -> Result<Value> {
        let element = set_element(ctx, set)?;
        self.set_binary_op(ctx, set, other, element, operation)
    }

    fn set_relation(
        &mut self,
        ctx: &mut Context,
        set: Value,
        other: &'s Expr,
        relation: SetRelation,
    ) -> Result<Value> {
        let element = set_element(ctx, set)?;
        self.set_relation_op(ctx, set, other, element, relation)
    }

    fn string_concat(&mut self, ctx: &mut Context, text: Value, args: &'s [Expr]) -> Result<Value> {
        let mut segments = Vec::with_capacity(args.len());
        for argument in args {
            let value = self.expression(ctx, argument)?;
            segments.push(self.display(ctx, value)?);
        }
        let mut result = text;
        for segment in segments {
            result = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, result, segment));
        }
        Ok(result)
    }

    fn string_normalize(
        &mut self,
        ctx: &mut Context,
        text: Value,
        form: Option<&'s Expr>,
    ) -> Result<Option<Value>> {
        let form = match form {
            None | Some(Expr::Undefined) => self.string(ctx, "NFC"),
            Some(arg) => self.string_operand(ctx, arg)?,
        };
        Ok(self.call_intrinsic(ctx, Intrinsic::StringNormalize, vec![text, form]))
    }

    fn locale_compare(&mut self, ctx: &mut Context, text: Value, other: &'s Expr) -> Result<Value> {
        let other = self.string_operand(ctx, other)?;
        self.compare_sign(ctx, text, other)
    }

    fn string_split(
        &mut self,
        ctx: &mut Context,
        text: Value,
        separator: &'s Expr,
    ) -> Result<Option<Value>> {
        let separator = self.expression(ctx, separator)?;
        let separator = self.defined(ctx, separator)?;
        let intrinsic = match kind(ctx, separator)? {
            Kind::String => Intrinsic::StringSplit,
            Kind::Regex => Intrinsic::RegexSplit,
            _ => bail!("string.split requires a string or literal regex separator"),
        };
        Ok(self.call_intrinsic(ctx, intrinsic, vec![text, separator]))
    }

    fn string_match(
        &mut self,
        ctx: &mut Context,
        text: Value,
        regex: &'s Expr,
    ) -> Result<Option<Value>> {
        let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
        Ok(self.call_intrinsic(ctx, Intrinsic::RegexMatch, vec![text, regex]))
    }

    fn string_search(
        &mut self,
        ctx: &mut Context,
        text: Value,
        regex: &'s Expr,
    ) -> Result<Option<Value>> {
        let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
        ensure!(
            !FunctionBuilder::regex_global(ctx, regex)?,
            "Regex search supports only the u and s flags"
        );
        Ok(self.call_intrinsic(ctx, Intrinsic::RegexSearch, vec![text, regex]))
    }

    fn regex_test_text(
        &mut self,
        ctx: &mut Context,
        regex: Value,
        text: &'s Expr,
    ) -> Result<Value> {
        ensure!(
            !FunctionBuilder::regex_global(ctx, regex)?,
            "Regex search supports only the u and s flags"
        );
        let text = self.string_operand(ctx, text)?;
        Ok(self.regex_test(ctx, regex, text))
    }

    fn regex_exec(
        &mut self,
        ctx: &mut Context,
        regex: Value,
        text: &'s Expr,
    ) -> Result<Option<Value>> {
        let text = self.string_operand(ctx, text)?;
        Ok(self.call_intrinsic(ctx, Intrinsic::RegexExec, vec![text, regex]))
    }

    fn search_params_append(
        &mut self,
        ctx: &mut Context,
        params: Value,
        name: &'s Expr,
        value: &'s Expr,
    ) -> Result<Value> {
        let name = self.string_operand(ctx, name)?;
        let value = self.string_operand(ctx, value)?;
        self.intrinsic_void(ctx, Intrinsic::UrlAppend, vec![params, name, value]);
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }

    fn abort_controller(&mut self, ctx: &mut Context, controller: Value) -> Result<Option<Value>> {
        self.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![controller]);
        Ok(None)
    }

    fn number_to_string(
        &mut self,
        ctx: &mut Context,
        number: Value,
        radix: Option<&'s Expr>,
    ) -> Result<Option<Value>> {
        let number = self.arithmetic(ctx, number);
        Ok(match radix {
            None | Some(Expr::Undefined) => {
                self.call_intrinsic(ctx, Intrinsic::NumberToString, vec![number])
            }
            Some(radix) => {
                let radix = self.number_operand(ctx, radix)?;
                self.call_intrinsic(ctx, Intrinsic::NumberToStringRadix, vec![number, radix])
            }
        })
    }

    fn bytes_set(
        &mut self,
        ctx: &mut Context,
        bytes: Value,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let source = args.first().context("set needs a source")?;
        let source = self.expression(ctx, source)?;
        let source = self.defined(ctx, source)?;
        let source = match kind(ctx, source)? {
            Kind::Bytes => source,
            Kind::Array => self.intrinsic(ctx, Intrinsic::BytesFromArray, vec![source]),
            _ => bail!("Uint8Array.set needs a Uint8Array or number[] source"),
        };
        let offset = self.number_or(ctx, args.get(1), 0.0)?;
        for arg in args.iter().skip(2) {
            self.expression(ctx, arg)?;
        }
        self.intrinsic_void(ctx, Intrinsic::BytesSetFrom, vec![bytes, source, offset]);
        Ok(None)
    }

    fn plain_date_equals(
        &mut self,
        ctx: &mut Context,
        date: Value,
        other: &'s Expr,
    ) -> Result<Value> {
        let other = self.coerce_plain_date(ctx, other, None)?;
        let left = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![date]);
        let right = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![other]);
        Ok(self.compare(ctx, CompareKind::Eq, left, right))
    }

    fn plain_date_until(
        &mut self,
        ctx: &mut Context,
        date: Value,
        other: &'s Expr,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let other = self.coerce_plain_date(ctx, other, None)?;
        self.temporal_until(ctx, date, other, options)
    }

    fn plain_date_since(
        &mut self,
        ctx: &mut Context,
        date: Value,
        other: &'s Expr,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let other = self.coerce_plain_date(ctx, other, None)?;
        self.temporal_until(ctx, other, date, options)
    }

    fn writer_write(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        chunk: &'s Expr,
    ) -> Result<Option<Value>> {
        let (chunk, _) = self.body_init(ctx, Some(chunk))?;
        let chunk = self.defined(ctx, chunk)?;
        Ok(self.call_intrinsic(ctx, Intrinsic::WriterWrite, vec![writer, chunk]))
    }

    fn readable_get_reader(
        &mut self,
        ctx: &mut Context,
        readable: Value,
        options: Option<&'s Expr>,
    ) -> Result<Option<Value>> {
        let intrinsic = if let Some(options) = options {
            let fields = self.init_fields(Some(options))?;
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
        Ok(self.call_intrinsic(ctx, intrinsic, vec![readable]))
    }

    fn reader_read_byob(
        &mut self,
        ctx: &mut Context,
        reader: Value,
        view: &'s Expr,
        options: Option<&'s Expr>,
    ) -> Result<Option<Value>> {
        let view = self.typed_operand(ctx, view, Kind::Bytes, "a Uint8Array")?;
        let fields = self.init_fields(options)?;
        ensure!(
            fields.is_empty() || (fields.len() == 1 && fields[0].0 == "min"),
            "BYOB read supports only {{min}}"
        );
        let minimum = self.number_or(ctx, fields.first().map(|(_, value)| *value), 1.0)?;
        Ok(self.call_intrinsic(ctx, Intrinsic::ReaderReadByob, vec![reader, view, minimum]))
    }
}
