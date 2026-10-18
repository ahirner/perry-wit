use super::*;
use crate::methods::arguments;

macro_rules! collection_calls {
    ($call:ident, $class:literal, $builder:ident, $ctx:ident, $receiver:ident, $method:ident;
        $types:pat = $resolve:expr;
        $( $variant:ident $name:literal [$($arg:ident),*] => $lower:expr, $requirement:literal; )*
    ) => {
        pub(super) enum $call<'s> {
            $( $variant($(collection_calls!(@argument $arg)),*), )*
        }

        impl<'s> $call<'s> {
            pub(super) fn parse(method: &str, args: &'s [Expr]) -> Result<Option<Self>> {
                Ok(Some(match method {
                    $( $name => {
                        let Some(($($arg,)*)) = arguments!(args; $($arg),*) else {
                            bail!(concat!($class, ".", $name, " ", $requirement));
                        };
                        Self::$variant($($arg),*)
                    }, )*
                    _ => return Ok(None),
                }))
            }

            pub(super) fn lower(
                self, $builder: &mut FunctionBuilder<'s>, $ctx: &mut Context, $receiver: Value,
            ) -> Result<Value> {
                let $method = match &self { $( Self::$variant(..) => $name, )* };
                let $types = $resolve.with_context(|| {
                    format!("{}.{method} receiver must be a {}", $class, $class, method = $method)
                })?;
                match self { $( Self::$variant($($arg),*) => $lower, )* }
            }
        }
    };
    (@argument $arg:ident) => { &'s Expr };
}

collection_calls! { MapCall, "Map", b, ctx, receiver, method;
    (key_type, value_type, _) = kv_entry_types(ctx, receiver.get_type(ctx));
    Get "get" [key] => {
        let key = b.expression_with(ctx, key, Some(key_type))?;
        b.kv_get(ctx, receiver, key, key_type, value_type)
    }, "requires one key argument";
    Set "set" [key, value] => {
        let key = b.expression_with(ctx, key, Some(key_type))?;
        let value = b.expression_with(ctx, value, Some(value_type))?;
        b.kv_set(ctx, receiver, key, value)
    }, "requires key and value arguments";
    Has "has" [key] => {
        let key = b.expression_with(ctx, key, Some(key_type))?;
        b.kv_has(ctx, receiver, key, key_type)
    }, "requires one key argument";
    Delete "delete" [key] => {
        let key = b.expression_with(ctx, key, Some(key_type))?;
        b.kv_delete(ctx, receiver, key, key_type)
    }, "requires one key argument";
    Clear "clear" [] => b.collection_clear(ctx, receiver), "takes no arguments";
    ForEach "forEach" [callback] => b.map_for_each(ctx, receiver, callback), "requires one callback argument";
}

collection_calls! { SetCall, "Set", b, ctx, receiver, method;
    (element, record) = set_entry_type(ctx, receiver.get_type(ctx));
    Add "add" [value] => {
        let value = b.expression_with(ctx, value, Some(element))?;
        b.set_add(ctx, receiver, value, element, record)
    }, "requires one value argument";
    Has "has" [value] => {
        let value = b.expression_with(ctx, value, Some(element))?;
        b.set_has(ctx, receiver, value, element)
    }, "requires one value argument";
    Delete "delete" [value] => {
        let value = b.expression_with(ctx, value, Some(element))?;
        b.set_delete(ctx, receiver, value, element)
    }, "requires one value argument";
    Clear "clear" [] => b.collection_clear(ctx, receiver), "takes no arguments";
    ForEach "forEach" [callback] => b.set_for_each(ctx, receiver, callback), "requires one callback argument";
    Difference "difference" [other] => b.set_binary_op(ctx, receiver, other, element, record, method), "requires one Set argument";
    Intersection "intersection" [other] => b.set_binary_op(ctx, receiver, other, element, record, method), "requires one Set argument";
    Union "union" [other] => b.set_binary_op(ctx, receiver, other, element, record, method), "requires one Set argument";
    SymmetricDifference "symmetricDifference" [other] => b.set_binary_op(ctx, receiver, other, element, record, method), "requires one Set argument";
    IsSubsetOf "isSubsetOf" [other] => b.set_relation_op(ctx, receiver, other, element, method), "requires one Set argument";
    IsSupersetOf "isSupersetOf" [other] => b.set_relation_op(ctx, receiver, other, element, method), "requires one Set argument";
    IsDisjointFrom "isDisjointFrom" [other] => b.set_relation_op(ctx, receiver, other, element, method), "requires one Set argument";
}
