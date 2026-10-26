//! Built-in method signatures and HIR call normalization.
use anyhow::{Result, bail};
use perry_hir::{
    ir::Expr,
    types::{LocalId, Type as HirType},
};
/// Source-level families used by built-in method signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Number,
    Boolean,
    String,
    BigInt,
    Bytes,
    Url,
    SearchParams,
    Json,
    Date,
    Stats,
    Headers,
    Response,
    Request,
    Writable,
    Writer,
    Readable,
    ByteBuffer,
    ByobReader,
    Reader,
    ReadResult,
    Instant,
    PlainDate,
    PlainDateTime,
    Abort,
    Incoming,
    Exception,
    Encoder,
    Decoder,
    Regex,
    ValueUnion,
    Array,
    Record,
    Variant,
    Resource,
    Settled,
}
impl Kind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::String => "string",
            Self::BigInt => "bigint",
            Self::Bytes => "Uint8Array",
            Self::Url => "URL",
            Self::SearchParams => "URLSearchParams",
            Self::Json => "JSON value",
            Self::Date => "Date",
            Self::Stats => "Stats",
            Self::Headers => "Headers",
            Self::Response => "Response",
            Self::Request => "Request",
            Self::Writable => "WritableStream",
            Self::Writer => "WritableStreamDefaultWriter",
            Self::Readable => "ReadableStream",
            Self::ByteBuffer => "ArrayBuffer",
            Self::ByobReader => "ReadableStreamBYOBReader",
            Self::Reader => "ReadableStreamDefaultReader",
            Self::ReadResult => "ReadableStreamReadResult",
            Self::Instant => "Temporal.Instant",
            Self::PlainDate => "Temporal.PlainDate",
            Self::PlainDateTime => "Temporal.PlainDateTime",
            Self::Abort => "AbortController",
            Self::Incoming => "IncomingRequest",
            Self::Exception => "Error",
            Self::Encoder => "TextEncoder",
            Self::Decoder => "TextDecoder",
            Self::Regex => "RegExp",
            Self::ValueUnion => "finite value union",
            Self::Array => "array",
            Self::Record => "record",
            Self::Variant => "variant",
            Self::Resource => "WIT resource",
            Self::Settled => "PromiseSettlementResult",
        }
    }
    pub(crate) fn builtin(name: &str) -> Option<Self> {
        match name {
            "Uint8Array" => Some(Self::Bytes),
            "URL" => Some(Self::Url),
            "URLSearchParams" => Some(Self::SearchParams),
            "Date" => Some(Self::Date),
            "Headers" => Some(Self::Headers),
            "Response" => Some(Self::Response),
            "Request" => Some(Self::Request),
            "WritableStream" => Some(Self::Writable),
            "WritableStreamDefaultWriter" => Some(Self::Writer),
            "ReadableStream" => Some(Self::Readable),
            "ArrayBuffer" => Some(Self::ByteBuffer),
            "ReadableStreamBYOBReader" => Some(Self::ByobReader),
            "ReadableStreamDefaultReader" => Some(Self::Reader),
            "ReadableStreamReadResult" => Some(Self::ReadResult),
            "Temporal.Instant" => Some(Self::Instant),
            "Temporal.PlainDate" => Some(Self::PlainDate),
            "Temporal.PlainDateTime" => Some(Self::PlainDateTime),
            "AbortController" | "AbortSignal" => Some(Self::Abort),
            "Error" | "TypeError" | "RangeError" | "ReferenceError" | "SyntaxError" => {
                Some(Self::Exception)
            }
            "TextEncoder" => Some(Self::Encoder),
            "TextDecoder" => Some(Self::Decoder),
            "RegExp" => Some(Self::Regex),
            _ => None,
        }
    }
}
pub(crate) const PUSH: &str = "$$perryArrayPush";
pub(crate) const CONCAT: &str = "$$perryArrayConcat";
pub(crate) const METHOD_PREFIX: &str = "$$perryMethod_";

enum Preservation {
    Upstream,
    Always,
    Push,
    Concat,
    Arguments,
}

impl Preservation {
    fn rewrite(&self, name: &str, count: usize, spread: bool) -> Option<String> {
        match self {
            Self::Push if count != 1 || spread => Some(PUSH.into()),
            Self::Concat => Some(CONCAT.into()),
            Self::Always => Some(format!("{METHOD_PREFIX}{name}")),
            Self::Arguments if count > 0 => Some(format!("{METHOD_PREFIX}{name}")),
            _ => None,
        }
    }
}

pub enum CallReceiver<'s> {
    Expression(&'s Expr),
    Local(LocalId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    Value(Kind),
    Map,
    Set,
}

impl Receiver {
    fn inferred(ty: &HirType) -> Option<Self> {
        Some(match ty {
            HirType::Array(_) | HirType::Tuple(_) => Self::Value(Kind::Array),
            HirType::String => Self::Value(Kind::String),
            HirType::Number => Self::Value(Kind::Number),
            HirType::Boolean => Self::Value(Kind::Boolean),
            HirType::Named(name) | HirType::Generic { base: name, .. } => match name.as_str() {
                "Map" | "ReadonlyMap" => Self::Map,
                "Set" | "ReadonlySet" => Self::Set,
                "Stats" | "fs.Stats" => Self::Value(Kind::Stats),
                _ => Self::Value(Kind::builtin(name)?),
            },
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodResult {
    Mapped,
    FlatMapped,
    Receiver,
    Flat,
    Element,
    At,
    Reduced,
    Void,
    String,
    Strings,
    NullableStrings,
    Number,
    Boolean,
    Lookup,
    Duration,
    OptionalNumber,
    NullableString,
    IteratorKeys,
    IteratorValues,
    IteratorEntries,
    Native(Kind),
    Promise(Kind),
    PromiseVoid,
    PromiseJson,
    Nullable(Kind),
    Optional(Kind),
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

macro_rules! argument_values {
    ($arg:ident) => {
        Some(*$arg).into_iter()
    };
    ($arg:ident:optional) => {
        $arg.iter().copied()
    };
    ($arg:ident:rest) => {
        $arg.iter()
    };
}

macro_rules! registry {
    ($( $family:pat, $class:literal {
            $( $variant:ident $($name:literal)|+ $result:ident $(($result_arg:expr))? $preserve:ident $form:ident
                [$($argument:ident $(:$mode:ident)?),*] => $action:ident $recipe:tt, $requirement:literal;
            )*
        } )*
    ) => {
        #[derive(Clone, Copy)]
        pub(crate) enum Method { $($($variant,)*)* }

        impl Method {
            pub(crate) fn lookup(receiver: Receiver, name: &str) -> Option<Self> {
                match (receiver, name) {
                    $($(($family, $($name)|+) => Some(Self::$variant),)*)*
                    _ => None,
                }
            }

            pub(crate) fn is_property(self) -> bool {
                match self { $($(Self::$variant => registry!(@property $form),)*)* }
            }

            fn result(self) -> MethodResult {
                match self { $($(Self::$variant => MethodResult::$result $(($result_arg))?,)*)* }
            }

            pub(crate) fn inferred(receiver: &HirType, name: &str, property: bool) -> Option<MethodResult> {
                if let Some(receiver) = Receiver::inferred(receiver) {
                    return Self::lookup(receiver, name).filter(|m| m.is_property() == property).map(Self::result);
                }
                if property { return None; }
                let mut result = None;
                $($(if registry!(@property $form) == property && matches!(name, $($name)|+) {
                    let rule = MethodResult::$result $(($result_arg))?;
                    if result.is_some_and(|previous| previous != rule) { return None; }
                    result = Some(rule);
                })*)*
                result
            }

            pub(crate) fn rewrite(name: &str, count: usize, spread: bool) -> Option<String> {
                $($(if matches!(name, $($name)|+) {
                    if let Some(rewritten) = Preservation::$preserve.rewrite(name, count, spread) {
                        return Some(rewritten);
                    }
                })*)*
                None
            }
        }

        pub enum Call<'s> {
            $($($variant($(argument_type!($argument $(:$mode)?)),*),)*)*
        }

        impl<'s> Call<'s> {
            pub fn parse(receiver: Receiver, name: &str, args: &'s [Expr]) -> Result<Option<Self>> {
                Self::member(receiver, name, Some(args))
            }

            pub fn member(receiver: Receiver, name: &str, args: Option<&'s [Expr]>) -> Result<Option<Self>> {
                let Some(method) = Method::lookup(receiver, name).filter(|m| m.is_property() == args.is_none()) else { return Ok(None) };
                let args = args.unwrap_or(&[]);
                Ok(Some(match method {
                    $($(Method::$variant => {
                        let Some(($($argument,)*)) = arguments!(args; $($argument $(:$mode)?),*) else {
                            bail!("{}.{} {}", $class, name, $requirement);
                        };
                        Self::$variant($($argument),*)
                    },)*)*
                }))
            }

            pub(crate) fn result(&self) -> MethodResult {
                match self { $($(Self::$variant(..) => MethodResult::$result $(($result_arg))?,)*)* }
            }

            pub(crate) fn argument(&self, index: usize) -> Option<&'s Expr> {
                match self {
                    $($(Self::$variant($($argument),*) => std::iter::empty()
                        $(.chain(argument_values!($argument $(:$mode)?)))*.nth(index),)*)*
                }
            }
        }
    };
    (@property Method) => { false };
    (@property Property) => { true };
}

mod registry;
crate::method_catalog!(registry);
impl<'s> Call<'s> {
    pub fn expression(expr: &'s Expr) -> Option<(CallReceiver<'s>, Self)> {
        match expr {
            Expr::SetAdd { set_id, value } => {
                return Some((CallReceiver::Local(*set_id), Self::SetAdd(value)));
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
        let (receiver, call): (&Expr, Self) = match expr {
            Expr::DateGetTime(date) | Expr::DateValueOf(date) => (date, Self::DateGetTime(&[])),
            Expr::DateToISOString(date) => (date, Self::DateToISOString(&[])),
            Expr::DateToJSON(date) => (date, Self::DateToJSON(&[])),
            Expr::UrlInstanceToString(url) | Expr::UrlInstanceToJSON(url) => {
                (url, Self::UrlString(&[]))
            }
            Expr::UrlSearchParamsAppend {
                params,
                name,
                value,
            } => (params, Self::SearchParamsAppend(name, value)),
            Expr::DateGetUtcFullYear(date) | Expr::DateGetFullYear(date) => {
                (date, Self::DateGetFullYear(&[]))
            }
            Expr::DateGetUtcMonth(date) | Expr::DateGetMonth(date) => {
                (date, Self::DateGetMonth(&[]))
            }
            Expr::DateGetUtcDate(date) | Expr::DateGetDate(date) => (date, Self::DateGetDate(&[])),
            Expr::DateGetUtcDay(date) | Expr::DateGetDay(date) => (date, Self::DateGetDay(&[])),
            Expr::DateGetUtcHours(date) | Expr::DateGetHours(date) => {
                (date, Self::DateGetHours(&[]))
            }
            Expr::DateGetUtcMinutes(date) | Expr::DateGetMinutes(date) => {
                (date, Self::DateGetMinutes(&[]))
            }
            Expr::DateGetUtcSeconds(date) | Expr::DateGetSeconds(date) => {
                (date, Self::DateGetSeconds(&[]))
            }
            Expr::DateGetUtcMilliseconds(date) | Expr::DateGetMilliseconds(date) => {
                (date, Self::DateGetMilliseconds(&[]))
            }
            Expr::DateGetTimezoneOffset(date) => (date, Self::DateGetTimezoneOffset(&[])),
            Expr::DateSetUtcFullYear { date, args } | Expr::DateSetFullYear { date, args } => {
                (date, Self::DateSetFullYear(args))
            }
            Expr::DateSetUtcMonth { date, args } | Expr::DateSetMonth { date, args } => {
                (date, Self::DateSetMonth(args))
            }
            Expr::DateSetUtcDate { date, args } | Expr::DateSetDate { date, args } => {
                (date, Self::DateSetDate(args))
            }
            Expr::DateSetUtcHours { date, args } | Expr::DateSetHours { date, args } => {
                (date, Self::DateSetHours(args))
            }
            Expr::DateSetUtcMinutes { date, args } | Expr::DateSetMinutes { date, args } => {
                (date, Self::DateSetMinutes(args))
            }
            Expr::DateSetUtcSeconds { date, args } | Expr::DateSetSeconds { date, args } => {
                (date, Self::DateSetSeconds(args))
            }
            Expr::DateSetUtcMilliseconds { date, args }
            | Expr::DateSetMilliseconds { date, args } => (date, Self::DateSetMilliseconds(args)),
            Expr::DateSetTime { date, args } => (date, Self::DateSetTime(args)),
            Expr::StringSplit(text, separator) => (text, Self::StringSplit(separator)),
            Expr::StringCodePointAt { string, index } => {
                (string, Self::StringCodePointAt(std::slice::from_ref(index)))
            }
            Expr::ArrayMap { array, callback } => (array, Self::ArrayMap(callback)),
            Expr::ArrayFilter { array, callback } => (array, Self::ArrayFilter(callback)),
            Expr::ArrayFlatMap { array, callback } => (array, Self::ArrayFlatMap(callback)),
            Expr::ArrayFind { array, callback } => (array, Self::ArrayFind(callback)),
            Expr::ArrayFindIndex { array, callback } => (array, Self::ArrayFindIndex(callback)),
            Expr::ArraySome { array, callback } => (array, Self::ArraySome(callback)),
            Expr::ArrayEvery { array, callback } => (array, Self::ArrayEvery(callback)),
            Expr::ArraySort { array, comparator } => (
                array,
                Self::ArraySort((!matches!(**comparator, Expr::Undefined)).then_some(comparator)),
            ),
            Expr::ArrayToSorted { array, comparator } => (
                array,
                Self::ArrayToSorted(
                    comparator
                        .as_deref()
                        .filter(|c| !matches!(c, Expr::Undefined)),
                ),
            ),
            Expr::ArrayIncludes {
                array,
                value,
                from_index,
            } => (array, Self::ArrayIncludes(value, from_index.as_deref())),
            Expr::ArrayIndexOf {
                array,
                value,
                from_index,
            } => (array, Self::ArrayIndexOf(value, from_index.as_deref())),
            Expr::MapClear(source) => (source, Self::MapClear()),
            Expr::SetClear(source) => (source, Self::SetClear()),
            Expr::MapKeys(source) | Expr::ArrayKeys(source) => (source, Self::CollectionKeys()),
            Expr::MapValues(source) | Expr::SetValues(source) | Expr::ArrayValues(source) => {
                (source, Self::CollectionValues())
            }
            Expr::MapEntries(source) | Expr::ArrayEntries(source) => {
                (source, Self::CollectionEntries())
            }
            Expr::MapGet { map, key } => (map, Self::MapGet(key)),
            Expr::MapSet { map, key, value } => (map, Self::MapSet(key, value)),
            Expr::MapHas { map, key } => (map, Self::MapHas(key)),
            Expr::MapDelete { map, key } => (map, Self::MapDelete(key)),
            Expr::SetHas { set, value } => (set, Self::SetHas(value)),
            Expr::SetDelete { set, value } => (set, Self::SetDelete(value)),
            _ => return None,
        };
        Some((CallReceiver::Expression(receiver), call))
    }
}
