//! Built-in method signatures and HIR call normalization.
use crate::hir_compat::Preservation;
use perry_hir::{ir::Expr, types::LocalId};
pub(crate) use perry_mir::ValueKind as Kind;
use perry_wit_diagnostics::Result;
use perry_wit_diagnostics::unsupported;

pub(crate) const URL_STRING_FIELDS: &[&str] = &[
    "href", "protocol", "pathname", "username", "password", "hash", "search", "host", "hostname",
    "port", "origin",
];

pub(crate) enum CallReceiver<'s> {
    Expression(&'s Expr),
    Local(LocalId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receiver {
    Value(Kind),
    Collection(crate::source::CollectionKind),
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
    Iterator(crate::collections::IteratorMethod),
    StreamReader,
}

macro_rules! bind_arguments {
    ($args:ident, $error:ident; $variant:ident; $all:ident:rest) => { Some(Call::$variant($args)) };
    ($args:ident, $error:ident; $variant:ident; $($name:ident $(:$mode:ident)?),*) => {{
        let mut remaining = $args.iter();
        $(let $name = bind_arguments!(@next remaining, $error $($mode)?);)*
        remaining.next().is_none().then_some(Call::$variant($($name),*))
    }};
    (@next $args:ident, $error:ident optional) => { $args.next() };
    (@next $args:ident, $error:ident) => { $args.next().ok_or_else($error)? };
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
    Collection,
}

pub(crate) enum ReplacementArg<T> {
    Value(T),
    Capture(T, u8),
    Groups(T),
}

impl<T: Copy> ReplacementArg<T> {
    pub(crate) fn layout(
        matched: T,
        offset: T,
        input: T,
        captures: Option<(T, (u8, bool))>,
    ) -> impl Iterator<Item = Self> {
        let numbered = captures.into_iter().flat_map(|(source, (count, _))| {
            (1..=count).map(move |index| Self::Capture(source, index))
        });
        let groups =
            captures.and_then(|(source, (_, named))| named.then_some(Self::Groups(source)));
        [Self::Value(matched)]
            .into_iter()
            .chain(numbered)
            .chain([Self::Value(offset), Self::Value(input)])
            .chain(groups)
    }
}

macro_rules! registry {
    (@result value (iterator_method(IteratorMethod::$method:ident));) => {
        MethodResult::Iterator(crate::collections::IteratorMethod::$method)
    };
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
    (@accepts_callback $name:ident, $pattern:pat, $kind:ident) => { matches!($name, $pattern) };
    (@accepts_callback $name:ident, $pattern:pat,) => { false };
    (@pattern $variant:ident $arguments:tt) => { Self::$variant(..) };
    (@pattern $variant:ident [$($arg:ident),*] $kind:ident) => { Self::$variant($($arg),*) };
    (@operands $arguments:tt) => { None };
    (@operands [$cb:ident] Comparator) => { $cb.map(|cb| (cb, None)) };
    (@operands [$cb:ident, $init:ident] Fold) => { Some(($cb, $init)) };
    (@operands [$search:ident, $cb:ident] Replacement) => { Some(($search, Some($cb))) };
    (@operands [$cb:ident] $kind:ident) => { Some(($cb, None)) };
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

            pub(crate) fn accepts_callback(name: &str) -> bool {
                false $($(|| registry!(@accepts_callback name, $($name)|+, $($callback)?))*)*
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
            pub(crate) fn callback_operands(&self) -> Option<(&'s Expr, Option<&'s Expr>)> {
                match *self {
                    $($(registry!(@pattern $variant [$($argument),*] $($callback)?) =>
                        registry!(@operands [$($argument),*] $($callback)?),)*)*
                }
            }

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
                match self {
                    $($(Self::$variant => {
                        let arity = || unsupported!("{}.{} {}", $class, name, $requirement);
                        bind_arguments!(args, arity; $variant; $($argument $(:$mode)?),*).ok_or_else(arity)
                    },)*)*
                }
            }
        }
    };
    (@property Method) => { false };
    (@property Property) => { true };
}

mod registry;
mod statics;
pub(crate) use statics::{StaticMethod, TemporalFactory};
crate::method_catalog!(registry);
impl<'s> Call<'s> {
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
