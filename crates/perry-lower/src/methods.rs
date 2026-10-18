use super::*;
use crate::array_methods::{Direction, FindResult, Mutation, Quantifier, SearchResult, Transform};

#[derive(Clone, Copy)]
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
    Collection,
    Lookup,
    Duration,
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

macro_rules! methods {
    ($builder:ident, $ctx:ident, $receiver:ident;
        arrays { $(
            $variant:ident $name:literal $result:ident $preserve:literal
            [$($argument:ident $(:$mode:ident)?),*] => $lower:expr, $requirement:literal;
        )* }
        other { $(
            $other:ident $other_name:literal $other_result:ident $other_preserve:literal;
        )* }
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(super) enum Method { $($variant,)* $($other,)* }

        impl Method {
            pub(super) fn parse(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    $($other_name => Some(Self::$other),)*
                    _ => None,
                }
            }

            pub(super) fn result(self) -> MethodResult {
                match self {
                    $(Self::$variant => MethodResult::$result,)*
                    $(Self::$other => MethodResult::$other_result,)*
                }
            }

            pub(super) fn preserve(self) -> bool {
                match self {
                    $(Self::$variant => $preserve,)*
                    $(Self::$other => $other_preserve,)*
                }
            }
        }

        impl<'s> FunctionBuilder<'s> {
            pub(super) fn array_method(
                &mut $builder, $ctx: &mut Context, $receiver: Value,
                method: Method, args: &'s [Expr],
            ) -> Result<Option<Value>> {
                match method {
                    $(Method::$variant => {
                        let Some(($($argument,)*)) = arguments!(args; $($argument $(:$mode)?),*) else {
                            bail!(concat!("array.", $name, " ", $requirement));
                        };
                        Ok(Some($lower?))
                    },)*
                    _ => Ok(None),
                }
            }
        }
    };
}

methods! { self, ctx, receiver;
    arrays {
        Push "push" Number false [args:rest] => self.push_values(ctx, receiver, args), "accepts values";
        Concat "concat" Receiver false [args:rest] => self.concat_values(ctx, receiver, args), "accepts values";
        Map "map" Mapped false [callback] => self.array_transform(ctx, receiver, callback, Transform::Map), "requires one static callback";
        Filter "filter" Receiver false [callback] => self.array_transform(ctx, receiver, callback, Transform::Filter), "requires one static callback";
        FlatMap "flatMap" FlatMapped false [callback] => self.array_transform(ctx, receiver, callback, Transform::FlatMap), "requires one static callback";
        Find "find" Element false [callback] => self.array_find(ctx, receiver, callback, FindResult::Value, Direction::Forward), "requires one static callback";
        FindIndex "findIndex" Number false [callback] => self.array_find(ctx, receiver, callback, FindResult::Index, Direction::Forward), "requires one static callback";
        FindLast "findLast" Element true [callback] => self.array_find(ctx, receiver, callback, FindResult::Value, Direction::Reverse), "requires one static callback";
        FindLastIndex "findLastIndex" Number true [callback] => self.array_find(ctx, receiver, callback, FindResult::Index, Direction::Reverse), "requires one static callback";
        Some "some" Boolean false [callback] => self.array_quantifier(ctx, receiver, callback, Quantifier::Some), "requires one static callback";
        Every "every" Boolean false [callback] => self.array_quantifier(ctx, receiver, callback, Quantifier::Every), "requires one static callback";
        ForEach "forEach" Void true [callback] => self.array_for_each(ctx, receiver, callback), "requires one static callback";
        Sort "sort" Receiver false [comparator:optional] => self.array_sort(ctx, receiver, comparator, Mutation::InPlace), "accepts at most one comparator callback";
        ToSorted "toSorted" Receiver false [comparator:optional] => self.array_sort(ctx, receiver, comparator, Mutation::Copy), "accepts at most one comparator callback";
        Reduce "reduce" Reduced true [callback, initial:optional] => self.array_reduce(ctx, receiver, callback, initial, Direction::Forward), "requires a callback and optional initial value";
        ReduceRight "reduceRight" Reduced true [callback, initial:optional] => self.array_reduce(ctx, receiver, callback, initial, Direction::Reverse), "requires a callback and optional initial value";
        Join "join" String true [separator:optional] => self.array_join(ctx, receiver, separator), "accepts at most one separator";
        Slice "slice" Receiver true [start:optional, end:optional] => self.array_slice(ctx, receiver, start, end), "accepts at most two bounds";
        Reverse "reverse" Receiver true [] => self.array_reverse(ctx, receiver, Mutation::InPlace), "takes no arguments";
        ToReversed "toReversed" Receiver true [] => self.array_reverse(ctx, receiver, Mutation::Copy), "takes no arguments";
        Flat "flat" Flat true [depth:optional] => self.array_flat(ctx, receiver, depth), "accepts at most one depth argument";
        At "at" At true [index:optional] => self.array_at(ctx, receiver, index), "accepts at most one index";
        Pop "pop" Element true [] => self.array_pop(ctx, receiver), "takes no arguments";
        With "with" Receiver true [index, value] => self.array_with(ctx, receiver, index, value), "requires index and value arguments";
        Includes "includes" Boolean false [value, from:optional] => self.array_search(ctx, receiver, value, from, SearchResult::Includes), "requires a value and optional starting index";
        IndexOf "indexOf" Number false [value, from:optional] => self.array_search(ctx, receiver, value, from, SearchResult::Index), "requires a value and optional starting index";
        LastIndexOf "lastIndexOf" Number true [value, from:optional] => self.array_last_index_of(ctx, receiver, value, from), "requires a value and optional starting index";
    }
    other {
        Replace "replace" String true;
        ReplaceAll "replaceAll" String true;
        PadStart "padStart" String true;
        PadEnd "padEnd" String true;
        Trim "trim" String false;
        TrimStart "trimStart" String true;
        TrimLeft "trimLeft" String true;
        TrimEnd "trimEnd" String true;
        TrimRight "trimRight" String true;
        ToString "toString" String false;
        Normalize "normalize" String true;
        Split "split" Strings true;
        Exec "exec" NullableStrings true;
        Match "match" NullableStrings true;
        LocaleCompare "localeCompare" Number false;
        Search "search" Number false;
        Has "has" Boolean false;
        Delete "delete" Boolean false;
        Is "is" Boolean false;
        IsArray "isArray" Boolean false;
        Test "test" Boolean false;
        Equals "equals" Boolean false;
        IsSubsetOf "isSubsetOf" Boolean true;
        IsSupersetOf "isSupersetOf" Boolean true;
        IsDisjointFrom "isDisjointFrom" Boolean true;
        Until "until" Duration false;
        Since "since" Duration false;
        Get "get" Lookup false;
        Set "set" Collection false;
        Add "add" Collection false;
        Subtract "subtract" Collection false;
        Union "union" Collection true;
        Intersection "intersection" Collection true;
        Difference "difference" Collection true;
        SymmetricDifference "symmetricDifference" Collection true;
    }
}
