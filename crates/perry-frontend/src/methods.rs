//! Built-in method signatures and HIR call normalization.
use crate::hir_compat::Preservation;
use perry_hir::{ir::Expr, types::LocalId};
use perry_wit_diagnostics::Result;
use perry_wit_diagnostics::bail_unsupported;
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::Context,
    r#type::TypeHandle,
};
/// Source-level value families, their diagnostic labels, optional builtin constructor names,
/// and corresponding MIR reference types.
#[macro_export]
macro_rules! kind_catalog {
    ($consumer:ident) => {
        $consumer! {
            primitives {
                Number "number";
                Boolean "boolean";
            }
            references {
                String "string" => StringType;
                BigInt "bigint" => BigIntType;
                Bytes "Uint8Array" builtin => BytesType;
                Url "URL" builtin => UrlType;
                SearchParams "URLSearchParams" builtin => SearchParamsType;
                Json "JSON value" => JsonType;
                Date "Date" builtin => DateType;
                Stats "Stats" => StatsType;
                Headers "Headers" builtin => HeadersType;
                Response "Response" builtin => ResponseType;
                Request "Request" builtin => RequestType;
                Writable "WritableStream" builtin => WritableType;
                Writer "WritableStreamDefaultWriter" builtin => WriterType;
                Readable "ReadableStream" builtin => ReadableType;
                ByteBuffer "ArrayBuffer" builtin => ByteBufferType;
                DataView "DataView" builtin => DataViewType;
                ByobReader "ReadableStreamBYOBReader" builtin => ByobReaderType;
                Reader "ReadableStreamDefaultReader" builtin => ReaderType;
                ReadResult "ReadableStreamReadResult" builtin => ReadResultType;
                Instant "Temporal.Instant" builtin => InstantType;
                PlainDate "Temporal.PlainDate" builtin => PlainDateType;
                PlainDateTime "Temporal.PlainDateTime" builtin => PlainDateTimeType;
                Abort "AbortController" builtin("AbortController" | "AbortSignal") => AbortType;
                Incoming "IncomingRequest" => IncomingType;
                Exception "Error" builtin("Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError") => ExceptionType;
                Encoder "TextEncoder" builtin => EncoderType;
                Decoder "TextDecoder" builtin => DecoderType;
                Regex "RegExp" builtin => RegexType;
                ValueUnion "finite value union" => ValueUnionType;
                Array "array" => ArrayType;
                Record "record" => RecordType;
                Variant "variant" => VariantType;
                Resource "WIT resource" => ResourceType;
                Settled "PromiseSettlementResult" => SettledType;
            }
        }
    };
}

macro_rules! define_kinds {
    (
        primitives { $($prim:ident $plabel:literal;)* }
        references { $($refk:ident $rlabel:literal $(builtin $(($($alias:literal)|+))?)? => $mir:ident;)* }
    ) => {
        /// Source-level families used by built-in method signatures.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Kind {
            $($prim,)*
            $($refk,)*
        }

        impl Kind {
            pub(crate) fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
                let ty = ty.deref(ctx);
                if ty.is::<FP64Type>() {
                    return Some(Self::Number);
                }
                if let Some(integer) = ty.downcast_ref::<IntegerType>() {
                    return match integer.width() {
                        1 => Some(Self::Boolean),
                        64 => Some(Self::BigInt),
                        _ => None,
                    };
                }
                $(if ty.is::<perry_mir::$mir>() { return Some(Self::$refk); })*
                None
            }

            pub(crate) const fn label(self) -> &'static str {
                match self {
                    $(Self::$prim => $plabel,)*
                    $(Self::$refk => $rlabel,)*
                }
            }

            pub(crate) fn builtin(name: &str) -> Option<Self> {
                $(define_kinds!(@builtin name $refk $rlabel $(builtin $(($($alias)|+))?)?);)*
                None
            }
        }
    };
    (@builtin $name:ident $kind:ident $label:literal builtin ($($alias:literal)|+)) => {
        if matches!($name, $($alias)|+) { return Some(Self::$kind); }
    };
    (@builtin $name:ident $kind:ident $label:literal builtin) => {
        if $name == $label { return Some(Self::$kind); }
    };
    (@builtin $name:ident $kind:ident $label:literal) => {};
}

crate::kind_catalog!(define_kinds);
pub(crate) enum CallReceiver<'s> {
    Expression(&'s Expr),
    Local(LocalId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receiver {
    Value(Kind),
    Map,
    Set,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodResult {
    Signature(&'static [perry_mir::SignatureType]),
    Mapped,
    FlatMapped,
    Receiver,
    Flat,
    Element,
    At,
    Reduced,
    Lookup,
    Duration,
    IteratorKeys,
    IteratorValues,
    IteratorEntries,
    StreamReader,
}

macro_rules! arguments {
    ($args:ident;) => {
        match $args {
            [] => Some(()),
            _ => None,
        }
    };
    ($args:ident; $all:ident:rest) => {
        Some(($args,))
    };
    ($args:ident; $first:ident) => {
        match $args {
            [first] => Some((first,)),
            _ => None,
        }
    };
    ($args:ident; $first:ident:optional) => {
        match $args {
            [] => Some((None,)),
            [first] => Some((Some(first),)),
            _ => None,
        }
    };
    ($args:ident; $first:ident, $second:ident) => {
        match $args {
            [first, second] => Some((first, second)),
            _ => None,
        }
    };
    ($args:ident; $first:ident, $second:ident:optional) => {
        match $args {
            [first] => Some((first, None)),
            [first, second] => Some((first, Some(second))),
            _ => None,
        }
    };
    ($args:ident; $first:ident:optional, $second:ident:optional) => {
        match $args {
            [] => Some((None, None)),
            [first] => Some((Some(first), None)),
            [first, second] => Some((Some(first), Some(second))),
            _ => None,
        }
    };
}

macro_rules! argument_type {
    ($name:ident) => { &'s Expr };
    ($name:ident:optional) => { Option<&'s Expr> };
    ($name:ident:rest) => { &'s [Expr] };
}

#[derive(Clone, Copy)]
pub(crate) enum Callback {
    Elements,
    AllIndices,
    Fold,
    Comparator,
    Replacement,
}

macro_rules! registry {
    (@result $action:ident $recipe:tt; [$($kind:ident)?]) => {
        MethodResult::Signature(&[$(perry_mir::SignatureType::$kind)?])
    };
    (@result $action:ident $recipe:tt; $rule:ident) => { MethodResult::$rule };
    (@result intrinsic ($op:ident, $args:expr, $inputs:tt, Receiver);) => { MethodResult::Receiver };
    (@result intrinsic ($op:ident, $args:expr, $inputs:tt, Found);) => { MethodResult::Signature(&[perry_mir::SignatureType::Bool]) };
    (@result intrinsic ($op:ident, $args:expr, $inputs:tt, Undefined);) => { MethodResult::Signature(&[]) };
    (@result intrinsic ($op:ident $(, $args:expr, $inputs:tt, Value)?);) => {
        MethodResult::Signature(perry_mir::Intrinsic::$op.result_kinds())
    };
    (@callback $kind:ident) => { Some(Callback::$kind) };
    (@callback) => { None };
    ($( $family:pat, $class:literal {
            $( $variant:ident $($name:literal)|+ $(-> $result:tt)? $preserve:ident $form:ident $(callback($callback:ident))?
                [$($argument:ident $(:$mode:ident)?),*] => $action:ident $recipe:tt, $requirement:literal;
            )*
        } )*
    ) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum Method { $($($variant,)*)* }

        impl Method {
            pub(crate) fn lookup(receiver: Receiver, name: &str) -> Option<Self> {
                match (receiver, name) {
                    $($(($family, $($name)|+) => Some(Self::$variant),)*)*
                    _ => None,
                }
            }

            pub(crate) fn callback(self) -> Option<Callback> {
                match self { $($(Self::$variant => registry!(@callback $($callback)?),)*)* }
            }

            pub(crate) fn is_property(self) -> bool {
                match self { $($(Self::$variant => registry!(@property $form),)*)* }
            }

            pub(crate) fn result_rule(self) -> MethodResult {
                match self { $($(Self::$variant => registry!(@result $action $recipe; $($result)?),)*)* }
            }

            pub(crate) fn unique_result_rule(name: &str) -> Option<MethodResult> {
                let mut result = None;
                $($(if !registry!(@property $form) && matches!(name, $($name)|+) {
                    let rule = Self::$variant.result_rule();
                    if result.is_some_and(|previous| previous != rule) { return None; }
                    result = Some(rule);
                })*)*
                result
            }

            pub(crate) fn preserve_arguments(name: &str, count: usize, spread: bool) -> bool {
                false $($(|| (matches!(name, $($name)|+) && Preservation::$preserve.needed(count, spread)))*)*
            }
        }

        pub(crate) enum Call<'s> {
            $($($variant($(argument_type!($argument $(:$mode)?)),*),)*)*
        }

        impl<'s> Call<'s> {
            pub(crate) fn method(&self) -> Method {
                match self { $($(Self::$variant(..) => Method::$variant,)*)* }
            }

            pub(crate) fn member(receiver: Receiver, name: &str, args: Option<&'s [Expr]>) -> Result<Option<Self>> {
                Method::lookup(receiver, name).filter(|m| m.is_property() == args.is_none())
                    .map(|m| m.bind(name, args.unwrap_or_default())).transpose()
            }
        }

        impl Method {
            pub(crate) fn bind<'s>(self, name: &str, args: &'s [Expr]) -> Result<Call<'s>> {
                Ok(match self {
                    $($(Self::$variant => {
                        let Some(($($argument,)*)) = arguments!(args; $($argument $(:$mode)?),*) else {
                            bail_unsupported!("{}.{} {}", $class, name, $requirement);
                        };
                        Call::$variant($($argument),*)
                    },)*)*
                })
            }
        }
    };
    (@property Method) => { false };
    (@property Property) => { true };
}

mod registry;
crate::method_catalog!(registry);
impl<'s> Call<'s> {
    pub(crate) fn array_callback(
        expr: &'s Expr,
    ) -> Option<(&'s Expr, Method, &'s Expr, Option<&'s Expr>)> {
        let (CallReceiver::Expression(array), call) = Self::expression(expr)? else {
            return None;
        };
        let method = call.method();
        let (callback, initial) = match call {
            Self::ArrayMap(cb)
            | Self::ArrayFilter(cb)
            | Self::ArrayFlatMap(cb)
            | Self::ArrayFind(cb)
            | Self::ArrayFindIndex(cb)
            | Self::ArraySome(cb)
            | Self::ArrayEvery(cb)
            | Self::ArrayForEach(cb)
            | Self::ArraySort(Some(cb))
            | Self::ArrayToSorted(Some(cb)) => (cb, None),
            Self::ArrayReduce(cb, initial) | Self::ArrayReduceRight(cb, initial) => (cb, initial),
            _ => return None,
        };
        Some((array, method, callback, initial))
    }

    pub(crate) fn expression(expr: &'s Expr) -> Option<(CallReceiver<'s>, Self)> {
        match expr {
            Expr::SetAdd { set_id, value } => {
                return Some((CallReceiver::Local(*set_id), Self::SetAdd(value)));
            }
            Expr::ArrayPop(array_id) => {
                return Some((CallReceiver::Local(*array_id), Self::ArrayPop()));
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                return Some((
                    CallReceiver::Local(*array_id),
                    Self::ArrayPush(std::slice::from_ref(value.as_ref())),
                ));
            }
            _ => {}
        }
        macro_rules! match_exprs {
            (
                date_get { $($dg_utc:ident | $dg_loc:ident => $dg_call:ident,)* }
                date_set { $($ds_utc:ident | $ds_loc:ident => $ds_call:ident,)* }
                array_cb { $($acb:ident,)* }
                map_key { $($mk:ident,)* }
                set_val { $($sv:ident,)* }
                rest { $($rest:tt)* }
            ) => {
                match expr {
                    $(Expr::$dg_utc(d) | Expr::$dg_loc(d) => (d, Self::$dg_call(&[])),)*
                    $(Expr::$ds_utc { date, args } | Expr::$ds_loc { date, args } => (date, Self::$ds_call(args)),)*
                    $(Expr::$acb { array, callback } => (array, Self::$acb(callback)),)*
                    $(Expr::$mk { map, key } => (map, Self::$mk(key)),)*
                    $(Expr::$sv { set, value } => (set, Self::$sv(value)),)*
                    $($rest)*
                }
            };
        }
        let (receiver, call): (&Expr, Self) = match_exprs! {
            date_get {
                DateGetUtcFullYear | DateGetFullYear => DateGetFullYear,
                DateGetUtcMonth | DateGetMonth => DateGetMonth,
                DateGetUtcDate | DateGetDate => DateGetDate,
                DateGetUtcDay | DateGetDay => DateGetDay,
                DateGetUtcHours | DateGetHours => DateGetHours,
                DateGetUtcMinutes | DateGetMinutes => DateGetMinutes,
                DateGetUtcSeconds | DateGetSeconds => DateGetSeconds,
                DateGetUtcMilliseconds | DateGetMilliseconds => DateGetMilliseconds,
            }
            date_set {
                DateSetUtcFullYear | DateSetFullYear => DateSetFullYear,
                DateSetUtcMonth | DateSetMonth => DateSetMonth,
                DateSetUtcDate | DateSetDate => DateSetDate,
                DateSetUtcHours | DateSetHours => DateSetHours,
                DateSetUtcMinutes | DateSetMinutes => DateSetMinutes,
                DateSetUtcSeconds | DateSetSeconds => DateSetSeconds,
                DateSetUtcMilliseconds | DateSetMilliseconds => DateSetMilliseconds,
            }
            array_cb {
                ArrayMap, ArrayFilter, ArrayFlatMap, ArrayFind, ArrayFindIndex, ArraySome, ArrayEvery, ArrayForEach,
            }
            map_key { MapGet, MapHas, MapDelete, }
            set_val { SetHas, SetDelete, }
            rest {
                Expr::CryptoRandomFillSync {
                    buffer,
                    offset,
                    size,
                } if matches!((&**offset, &**size), (Expr::Undefined, Expr::Undefined)) => {
                    (buffer, Self::BytesCryptoFillRandom(&[]))
                }
                Expr::DateGetTime(date) => (date, Self::DateGetTime(&[])),
                Expr::DateToISOString(date) => (date, Self::DateToISOString(&[])),
                Expr::DateToJSON(date) => (date, Self::DateToJSON(&[])),
                Expr::DateGetTimezoneOffset(date) => (date, Self::DateGetTimezoneOffset(&[])),
                Expr::DateSetTime { date, args } => (date, Self::DateSetTime(args)),
                Expr::UrlInstanceToString(url) | Expr::UrlInstanceToJSON(url) => {
                    (url, Self::UrlString(&[]))
                }
                Expr::UrlSearchParamsAppend { params, name, value } => {
                    (params, Self::SearchParamsAppend(name, value))
                }
                Expr::StringSplit(text, separator) => (text, Self::StringSplit(separator)),
                Expr::ArraySlice { array, start, end, .. } => {
                    (array, Self::ArraySlice(Some(start), end.as_deref()))
                }
                Expr::ArrayToReversed { array } => (array, Self::ArrayToReversed()),
                Expr::ArrayWith { array, index, value } => (array, Self::ArrayWith(index, value)),
                Expr::ArrayAt { array, index } => (array, Self::ArrayAt(Some(index))),
                Expr::ArrayReduce { array, callback, initial } => (array, Self::ArrayReduce(callback, initial.as_deref())),
                Expr::ArrayReduceRight { array, callback, initial } => (array, Self::ArrayReduceRight(callback, initial.as_deref())),
                Expr::ArraySort { array, comparator } => (
                    array,
                    Self::ArraySort((!matches!(**comparator, Expr::Undefined)).then_some(comparator)),
                ),
                Expr::ArrayToSorted { array, comparator } => (
                    array,
                    Self::ArrayToSorted(
                        comparator.as_deref().filter(|c| !matches!(c, Expr::Undefined)),
                    ),
                ),
                Expr::ArrayIncludes { array, value, from_index } => {
                    (array, Self::ArrayIncludes(value, from_index.as_deref()))
                }
                Expr::ArrayIndexOf { array, value, from_index } => {
                    (array, Self::ArrayIndexOf(value, from_index.as_deref()))
                }
                Expr::MapClear(source) => (source, Self::MapClear()),
                Expr::SetClear(source) => (source, Self::SetClear()),
                Expr::MapKeys(source) | Expr::ArrayKeys(source) => (source, Self::CollectionKeys()),
                Expr::MapValues(source) | Expr::SetValues(source) | Expr::ArrayValues(source) => {
                    (source, Self::CollectionValues())
                }
                Expr::MapEntries(source) | Expr::ArrayEntries(source) => {
                    (source, Self::CollectionEntries())
                }
                Expr::MapSet { map, key, value } => (map, Self::MapSet(key, value)),
                _ => return None,
            }
        };
        Some((CallReceiver::Expression(receiver), call))
    }
}

/// Positional captures and the optional groups argument in a replacement callback.
pub(crate) fn regex_captures(pattern: &str) -> (usize, bool) {
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    let mut count = 0;
    let mut named = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                chars.next();
            }
            '[' => in_class = true,
            ']' => in_class = false,
            '(' if !in_class => {
                if chars.peek() != Some(&'?') {
                    count += 1;
                } else {
                    chars.next();
                    if chars.next() == Some('<') && !matches!(chars.peek(), Some('=' | '!')) {
                        count += 1;
                        named = true;
                    }
                }
            }
            _ => {}
        }
    }
    (count, named)
}
