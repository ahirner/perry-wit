//! Built-in method metadata shared by lowering, inference and HIR name preservation.
use super::*;
use crate::array_methods::{Direction, FindResult, Mutation, Quantifier, Search, Transform};
use crate::collections::{SetOperation, SetRelation, kv_entry_types, set_entry_type};

mod intrinsic;
use intrinsic::{
    Argument::{Constant, Number, Padding, Required, SearchString},
    IntrinsicCall, Return,
};

pub(super) const PUSH: &str = "$$perryArrayPush";
pub(super) const CONCAT: &str = "$$perryArrayConcat";
pub(super) const METHOD_PREFIX: &str = "$$perryMethod_";

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

pub(super) enum CallReceiver<'s> {
    Expression(&'s Expr),
    Local(LocalId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Receiver {
    Value(Kind),
    Map,
    Set,
}

impl Receiver {
    pub(super) fn of(ctx: &Context, value: Value) -> Result<Self> {
        let ty = value.get_type(ctx);
        Ok(if crate::collections::map_entry_types(ctx, ty).is_some() {
            Self::Map
        } else if set_entry_type(ctx, ty).is_some() {
            Self::Set
        } else {
            Self::Value(kind(ctx, value)?)
        })
    }

    fn inferred(ctx: &Context, ty: &HirType) -> Option<Self> {
        Some(match ty {
            HirType::Array(_) | HirType::Tuple(_) => Self::Value(Kind::Array),
            HirType::String => Self::Value(Kind::String),
            HirType::Number => Self::Value(Kind::Number),
            HirType::Boolean => Self::Value(Kind::Boolean),
            HirType::Named(name) | HirType::Generic { base: name, .. } => match name.as_str() {
                "Map" | "ReadonlyMap" => Self::Map,
                "Set" | "ReadonlySet" => Self::Set,
                "Stats" | "fs.Stats" => Self::Value(Kind::Stats),
                _ => Self::Value(Kind::of(ctx, Kind::builtin_type(ctx, name)?)?),
            },
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodResult {
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
    ($builder:ident, $ctx:ident, $receiver:ident;
        $( $family:pat, $class:literal $setup:tt {
            $( $variant:ident $($name:literal)|+ $result:ident $(($result_arg:expr))? $preserve:ident $form:ident
                [$($argument:ident $(:$mode:ident)?),*] => $action:ident $lower:tt, $requirement:literal;
            )*
        } )*
    ) => {
        #[derive(Clone, Copy)]
        pub(super) enum Method { $($($variant,)*)* }

        impl Method {
            pub(super) fn lookup(receiver: Receiver, name: &str) -> Option<Self> {
                match (receiver, name) {
                    $($(($family, $($name)|+) => Some(Self::$variant),)*)*
                    _ => None,
                }
            }

            pub(super) fn is_property(self) -> bool {
                match self { $($(Self::$variant => registry!(@property $form),)*)* }
            }

            pub(super) fn result(self) -> MethodResult {
                match self { $($(Self::$variant => MethodResult::$result $(($result_arg))?,)*)* }
            }

            pub(super) fn inferred(ctx: &Context, receiver: &HirType, name: &str, property: bool) -> Option<MethodResult> {
                if let Some(receiver) = Receiver::inferred(ctx, receiver) {
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

            pub(super) fn rewrite(name: &str, count: usize, spread: bool) -> Option<String> {
                $($(if matches!(name, $($name)|+) {
                    if let Some(rewritten) = Preservation::$preserve.rewrite(name, count, spread) {
                        return Some(rewritten);
                    }
                })*)*
                None
            }
        }

        pub(super) enum Call<'s> {
            $($($variant($(argument_type!($argument $(:$mode)?)),*),)*)*
        }

        impl<'s> Call<'s> {
            pub(super) fn parse(receiver: Receiver, name: &str, args: &'s [Expr]) -> Result<Option<Self>> {
                Self::member(receiver, name, Some(args))
            }

            pub(super) fn member(receiver: Receiver, name: &str, args: Option<&'s [Expr]>) -> Result<Option<Self>> {
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

            pub(super) fn result(&self) -> MethodResult {
                match self { $($(Self::$variant(..) => MethodResult::$result $(($result_arg))?,)*)* }
            }

            pub(super) fn argument(&self, index: usize) -> Option<&'s Expr> {
                match self {
                    $($(Self::$variant($($argument),*) => std::iter::empty()
                        $(.chain(argument_values!($argument $(:$mode)?)))*.nth(index),)*)*
                }
            }

            pub(super) fn lower(
                self, $builder: &mut FunctionBuilder<'s>, $ctx: &mut Context, $receiver: Value,
            ) -> Result<Option<Value>> {
                match self { $($(Self::$variant($($argument),*) => registry!(@lower $setup ($builder, $ctx, $receiver) $action $lower),)*)* }
            }
        }
    };
    (@property Method) => { false };
    (@property Property) => { true };
    (@lower { $($setup:tt)* } ($b:ident, $ctx:ident, $receiver:ident) $action:ident $lower:tt) => {{
        $($setup)* registry!(@invoke ($b, $ctx, $receiver) $action $lower)
    }};
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
    (@invoke ($b:ident, $ctx:ident, $receiver:ident) intrinsic ($intrinsic:ident, $args:expr, [$($argument:expr),*], $result:ident)) => {
        IntrinsicCall { intrinsic: Intrinsic::$intrinsic, arguments: &[$($argument),*], result: Return::$result }
            .lower($b, $ctx, $receiver, $args)
    };
}

include!("methods/registry.rs");
impl<'s> Call<'s> {
    pub(super) fn expression(expr: &'s Expr) -> Option<(CallReceiver<'s>, Self)> {
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
