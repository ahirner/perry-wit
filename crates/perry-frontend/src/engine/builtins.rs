//! Built-in expression, method, property, constructor, and callback typing over dense `Ty`.
//!
//! Operates exclusively on `Ty` indices without cloning `HirType` trees or delegating composite
//! expressions to `perry_hir::infer_expr_type`.
use super::types::{NameId, ObjectProp, Ty, TyKind, TypeTable};
use crate::{
    Absence,
    collections::IteratorMethod,
    hir_compat::{self, Constructor},
    methods::{Callback, Kind, Method, MethodResult, Receiver},
    shapes::{self, Declarations},
};
use perry_hir::{
    ir::{BinaryOp, Expr, LogicalOp, UnaryOp},
    types::Type as HirType,
};
use pliron::context::Context;

pub(crate) const SETTLED: &str = "PromiseSettledResult";

/// Built-in callback invocation site recognized by the single callback table.
pub(crate) struct BuiltinCallback<'h> {
    pub(crate) callback: &'h Expr,
    pub(crate) inputs: Vec<Ty>,
    pub(crate) contract: CallbackContract,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallbackContract {
    /// Result discarded (`forEach`).
    Void,
    /// Returns mapped element (`map`, `Array.from`).
    Mapped { keep_holes: bool },
    /// Returns flattened array (`flatMap`).
    FlatMapped,
    /// Returns predicate (`filter`, `some`, `every`, `find`, `findIndex`, `findLast`, `findLastIndex`).
    Predicate,
    /// Returns comparator number (`sort`, `toSorted`).
    Comparator,
    /// Returns next accumulator (`reduce`, `reduceRight`).
    Fold,
    /// Returns replacement string (`replace`, `replaceAll`).
    Replacement,
    /// Returns map key (`Map.groupBy`).
    GroupKey,
}

/// Normalize a leaf `HirType::Named` integer carrier (64-bit -> BigInt, <=32-bit -> Number).
pub(crate) fn normalize_named_integer(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &TypeTable,
    ty: Ty,
) -> Ty {
    if let TyKind::Leaf(id) = types.kind(ty)
        && let HirType::Named(name) = id.hir(ctx)
        && let Some(carrier) = decls.integer_carrier(name)
    {
        return match carrier {
            HirType::BigInt => Ty::BIGINT,
            _ => Ty::NUMBER,
        };
    }
    ty
}

/// Map a `Ty` to a built-in [`Receiver`] family for method/property lookup.
pub(crate) fn receiver_of(ctx: &Context, types: &TypeTable, ty: Ty) -> Option<Receiver> {
    match types.kind(ty) {
        TyKind::Array(_) | TyKind::Tuple(_) => Some(Receiver::Value(Kind::Array)),
        TyKind::String => Some(Receiver::Value(Kind::String)),
        TyKind::Number | TyKind::Int32 => Some(Receiver::Value(Kind::Number)),
        TyKind::Boolean => Some(Receiver::Value(Kind::Boolean)),
        TyKind::BigInt => Some(Receiver::Value(Kind::BigInt)),
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Receiver::Value(Kind::String)),
            HirType::Named(name) => receiver_for_name(name),
            _ => None,
        },
        TyKind::Generic { base, .. } => match base.hir(ctx) {
            HirType::Named(name) => receiver_for_name(name),
            _ => None,
        },
        _ => None,
    }
}

fn receiver_for_name(name: &str) -> Option<Receiver> {
    match name {
        "Map" | "ReadonlyMap" => Some(Receiver::Map),
        "Set" | "ReadonlySet" => Some(Receiver::Set),
        "Stats" | "fs.Stats" => Some(Receiver::Value(Kind::Stats)),
        other => Some(Receiver::Value(Kind::builtin(other)?)),
    }
}

/// Look up a built-in method/property result rule for a `Ty` receiver.
pub(crate) fn lookup_method_result(
    ctx: &Context,
    types: &TypeTable,
    receiver: Ty,
    name: &str,
    property: bool,
) -> Option<MethodResult> {
    if let Some(recv) = receiver_of(ctx, types, receiver) {
        return Method::lookup(recv, name)
            .filter(|m| m.is_property() == property)
            .map(method_result_of);
    }
    if property {
        return None;
    }
    // Fall back to unique-result lookup only when receiver is not a known built-in family.
    let hir = types.materialize_hir(ctx, receiver, &|_| None);
    Method::inferred(&hir, name, false)
}

fn method_result_of(method: Method) -> MethodResult {
    // Reuse Method::inferred via a direct helper on known receiver; wait: `Method::result` is private.
    // Call `Method::inferred` using the receiver's canonical HirType or map directly.
    let _ = method;
    // Wait: we expose `pub(crate) fn result(self) -> MethodResult` on `Method` in `methods.rs`!
    method.result_rule()
}

/// Extract the iterated element type of an array, string, tuple, Set, Map, or typed array.
pub(crate) fn iterated_element(ctx: &Context, types: &mut TypeTable, ty: Ty) -> Option<Ty> {
    let (ty, _) = types.strip_nullish(ty);
    match types.kind(ty) {
        TyKind::String => Some(Ty::STRING),
        TyKind::Array(elem) => Some(elem),
        TyKind::Tuple(span) => {
            let items: Vec<Ty> = types.tuple_slice(span).to_vec();
            Some(types.union(&items))
        }
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::StringLiteral(_) => Some(Ty::STRING),
            HirType::Named(name) if name == "Uint8Array" => Some(Ty::NUMBER),
            _ => None,
        },
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            let slice: Vec<Ty> = types.generic_slice(args).to_vec();
            match (base_name.as_str(), slice.as_slice()) {
                ("Set" | "ReadonlySet" | "SetValues" | "Array" | "ReadonlyArray", [elem]) => {
                    Some(*elem)
                }
                ("MapKeys", [key, _]) => Some(*key),
                ("MapValues", [_, val]) => Some(*val),
                ("Map" | "ReadonlyMap" | "MapEntries", [key, val]) => {
                    Some(types.tuple(&[*key, *val]))
                }
                ("SetEntries", [elem]) => Some(types.tuple(&[*elem, *elem])),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Result type of `.keys()`, `.values()`, `.entries()` on Map, Set, or Array.
pub(crate) fn iterator_view(
    ctx: &Context,
    types: &mut TypeTable,
    source: Ty,
    method: IteratorMethod,
) -> Option<Ty> {
    let (source, _) = types.strip_nullish(source);
    match types.kind(source) {
        TyKind::Array(elem) => {
            let out_elem = match method {
                IteratorMethod::Keys => Ty::NUMBER,
                IteratorMethod::Values => elem,
                IteratorMethod::Entries => types.tuple(&[Ty::NUMBER, elem]),
            };
            Some(types.array(out_elem))
        }
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            let slice: Vec<Ty> = types.generic_slice(args).to_vec();
            match base_name.as_str() {
                "Map" | "ReadonlyMap" if slice.len() == 2 => {
                    let view_base = shapes::TypeId::from_owned(
                        ctx,
                        HirType::Named(method.map_view().type_name().to_owned()),
                    );
                    Some(types.generic(view_base, &slice))
                }
                "Set" | "ReadonlySet" if !slice.is_empty() => {
                    let view_base = shapes::TypeId::from_owned(
                        ctx,
                        HirType::Named(method.set_view().type_name().to_owned()),
                    );
                    Some(types.generic(view_base, &slice[..1]))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Compute the inputs and contract of any built-in callback call expression.
pub(crate) fn builtin_callback<'h>(
    ctx: &Context,
    types: &mut TypeTable,
    sparse_arrays: bool,
    expr: &'h Expr,
    eval: &mut impl FnMut(&'h Expr, &mut TypeTable) -> Ty,
) -> Option<BuiltinCallback<'h>> {
    match expr {
        Expr::ArrayMap { array, callback } => {
            let arr_ty = eval(array, types);
            let (recv, elem) = array_receiver(types, arr_ty)?;
            Some(BuiltinCallback {
                callback,
                inputs: vec![elem, Ty::NUMBER, recv],
                contract: CallbackContract::Mapped {
                    keep_holes: sparse_arrays,
                },
            })
        }
        Expr::ArrayFlatMap { array, callback } => {
            let arr_ty = eval(array, types);
            let (recv, elem) = array_receiver(types, arr_ty)?;
            Some(BuiltinCallback {
                callback,
                inputs: vec![elem, Ty::NUMBER, recv],
                contract: CallbackContract::FlatMapped,
            })
        }
        Expr::ArrayFilter { array, callback }
        | Expr::ArraySome { array, callback }
        | Expr::ArrayEvery { array, callback } => {
            let arr_ty = eval(array, types);
            let (recv, elem) = array_receiver(types, arr_ty)?;
            Some(BuiltinCallback {
                callback,
                inputs: vec![elem, Ty::NUMBER, recv],
                contract: CallbackContract::Predicate,
            })
        }
        Expr::ArrayFind { array, callback } | Expr::ArrayFindIndex { array, callback } => {
            let arr_ty = eval(array, types);
            let (recv, elem) = array_receiver(types, arr_ty)?;
            let opt_elem = types.with_absences(elem, Absence::Undefined);
            Some(BuiltinCallback {
                callback,
                inputs: vec![opt_elem, Ty::NUMBER, recv],
                contract: CallbackContract::Predicate,
            })
        }
        Expr::ArraySort { array, comparator } => {
            if matches!(comparator.as_ref(), Expr::Undefined) {
                return None;
            }
            let arr_ty = eval(array, types);
            let (_, elem) = array_receiver(types, arr_ty)?;
            let defined = types.exclude_absences(elem, Absence::Undefined);
            Some(BuiltinCallback {
                callback: comparator,
                inputs: vec![defined, defined],
                contract: CallbackContract::Comparator,
            })
        }
        Expr::ArrayToSorted {
            array, comparator, ..
        } => {
            let comparator = comparator
                .as_deref()
                .filter(|c| !matches!(c, Expr::Undefined))?;
            let arr_ty = eval(array, types);
            let (_, elem) = array_receiver(types, arr_ty)?;
            let defined = types.exclude_absences(elem, Absence::Undefined);
            Some(BuiltinCallback {
                callback: comparator,
                inputs: vec![defined, defined],
                contract: CallbackContract::Comparator,
            })
        }
        Expr::ArrayFromMapped {
            iterable, map_fn, ..
        } => {
            let src_ty = eval(iterable, types);
            let elem = if let Expr::Object(props) = iterable.as_ref()
                && props.iter().any(|(k, _)| k == "length")
            {
                Ty::VOID
            } else {
                iterated_element(ctx, types, src_ty).unwrap_or(Ty::OPEN)
            };
            Some(BuiltinCallback {
                callback: map_fn,
                inputs: vec![elem, Ty::NUMBER],
                contract: CallbackContract::Mapped { keep_holes: false },
            })
        }
        Expr::MapGroupBy { items, key_fn } => {
            let src_ty = eval(items, types);
            let elem = iterated_element(ctx, types, src_ty).unwrap_or(Ty::OPEN);
            Some(BuiltinCallback {
                callback: key_fn,
                inputs: vec![elem, Ty::NUMBER],
                contract: CallbackContract::GroupKey,
            })
        }
        Expr::Call { callee, args, .. } => {
            let Expr::PropertyGet {
                object, property, ..
            } = callee.as_ref()
            else {
                return None;
            };
            let method_name = hir_compat::method(property).name();
            // Static Map.groupBy(items, keyFn)
            if let Expr::PropertyGet {
                object: global,
                property: class,
                ..
            } = object.as_ref()
                && matches!(global.as_ref(), Expr::GlobalGet(_))
                && class == "Map"
                && method_name == "groupBy"
            {
                let [items, key_fn] = args.as_slice() else {
                    return None;
                };
                let src_ty = eval(items, types);
                let elem = iterated_element(ctx, types, src_ty).unwrap_or(Ty::OPEN);
                return Some(BuiltinCallback {
                    callback: key_fn,
                    inputs: vec![elem, Ty::NUMBER],
                    contract: CallbackContract::GroupKey,
                });
            }

            let obj_ty = eval(object, types);
            let (stripped, _) = types.strip_nullish(obj_ty);

            // Map.forEach / Set.forEach
            if method_name == "forEach"
                && let TyKind::Generic { base, args: gargs } = types.kind(stripped)
                && let HirType::Named(base_name) = base.hir(ctx)
            {
                let callback = args.first()?;
                let gslice: Vec<Ty> = types.generic_slice(gargs).to_vec();
                match (base_name.as_str(), gslice.as_slice()) {
                    ("Map" | "ReadonlyMap", [k, v]) => {
                        return Some(BuiltinCallback {
                            callback,
                            inputs: vec![*v, *k, stripped],
                            contract: CallbackContract::Void,
                        });
                    }
                    ("Set" | "ReadonlySet", [v]) => {
                        return Some(BuiltinCallback {
                            callback,
                            inputs: vec![*v, *v, stripped],
                            contract: CallbackContract::Void,
                        });
                    }
                    _ => {}
                }
            }

            let recv_kind = receiver_of(ctx, types, stripped)?;
            let method = Method::lookup(recv_kind, method_name)?;
            let cb_kind = method.callback()?;
            let (first_arg, tail) = args.split_first()?;
            match cb_kind {
                Callback::Replacement => {
                    let callback = tail.first()?;
                    if !matches!(
                        callback,
                        Expr::Closure { .. } | Expr::FuncRef(_) | Expr::LocalGet(_)
                    ) {
                        return None;
                    }
                    let mut inputs = vec![Ty::STRING];
                    let captures = match first_arg {
                        Expr::RegExp { pattern, .. } => {
                            Some(crate::methods::regex_captures(pattern))
                        }
                        Expr::String(_) => Some((0, false)),
                        _ => None,
                    };
                    if let Some((count, named)) = captures {
                        let opt_str = types.with_absences(Ty::STRING, Absence::Undefined);
                        inputs.extend(std::iter::repeat_n(opt_str, count));
                        inputs.extend([Ty::NUMBER, Ty::STRING]);
                        if named {
                            inputs.push(Ty::DYNAMIC);
                        }
                    }
                    Some(BuiltinCallback {
                        callback,
                        inputs,
                        contract: CallbackContract::Replacement,
                    })
                }
                Callback::Elements => {
                    let (recv, elem) = array_receiver(types, stripped)?;
                    let contract = match method_name {
                        "forEach" => CallbackContract::Void,
                        "map" => CallbackContract::Mapped {
                            keep_holes: sparse_arrays,
                        },
                        "flatMap" => CallbackContract::FlatMapped,
                        _ => CallbackContract::Predicate,
                    };
                    Some(BuiltinCallback {
                        callback: first_arg,
                        inputs: vec![elem, Ty::NUMBER, recv],
                        contract,
                    })
                }
                Callback::AllIndices => {
                    let (recv, elem) = array_receiver(types, stripped)?;
                    let opt_elem = types.with_absences(elem, Absence::Undefined);
                    Some(BuiltinCallback {
                        callback: first_arg,
                        inputs: vec![opt_elem, Ty::NUMBER, recv],
                        contract: CallbackContract::Predicate,
                    })
                }
                Callback::Comparator => {
                    if matches!(first_arg, Expr::Undefined) {
                        return None;
                    }
                    let (_, elem) = array_receiver(types, stripped)?;
                    let defined = types.exclude_absences(elem, Absence::Undefined);
                    Some(BuiltinCallback {
                        callback: first_arg,
                        inputs: vec![defined, defined],
                        contract: CallbackContract::Comparator,
                    })
                }
                Callback::Fold => {
                    let (recv, elem) = array_receiver(types, stripped)?;
                    let acc = tail.first().map_or(elem, |initial| eval(initial, types));
                    Some(BuiltinCallback {
                        callback: first_arg,
                        inputs: vec![acc, elem, Ty::NUMBER, recv],
                        contract: CallbackContract::Fold,
                    })
                }
            }
        }
        _ => None,
    }
}

fn array_receiver(types: &mut TypeTable, ty: Ty) -> Option<(Ty, Ty)> {
    let (stripped, _) = types.strip_nullish(ty);
    match types.kind(stripped) {
        TyKind::Array(elem) => Some((stripped, elem)),
        TyKind::Tuple(span) => {
            let items: Vec<Ty> = types.tuple_slice(span).to_vec();
            let elem = types.union(&items);
            let arr = types.array(elem);
            Some((arr, elem))
        }
        _ => None,
    }
}

/// Arguments to [`apply_method_result`].
#[derive(Clone, Copy)]
pub(crate) struct MethodResultArgs<'a> {
    pub(crate) receiver: Ty,
    pub(crate) arg0: Option<Ty>,
    pub(crate) arg0_expr: Option<&'a Expr>,
    pub(crate) callback_result: Option<Ty>,
    pub(crate) sparse_arrays: bool,
}

impl MethodResultArgs<'_> {
    pub(crate) fn receiver_only(receiver: Ty) -> Self {
        Self {
            receiver,
            arg0: None,
            arg0_expr: None,
            callback_result: None,
            sparse_arrays: false,
        }
    }
}

/// Evaluate the result type of a [`MethodResult`] rule given already-typed receiver and callback/arg types.
pub(crate) fn apply_method_result(
    ctx: &Context,
    types: &mut TypeTable,
    rule: MethodResult,
    call: MethodResultArgs<'_>,
) -> Ty {
    let MethodResultArgs {
        receiver,
        arg0,
        arg0_expr,
        callback_result,
        sparse_arrays,
    } = call;
    match rule {
        MethodResult::Mapped => match callback_result {
            Some(ret) if !types.is_open(ret) => {
                let elem = if sparse_arrays {
                    types.with_absences(ret, Absence::Undefined)
                } else {
                    ret
                };
                types.array(elem)
            }
            _ => Ty::OPEN,
        },
        MethodResult::FlatMapped => match callback_result {
            Some(ret) if !types.is_open(ret) => match types.kind(ret) {
                TyKind::Array(_) => ret,
                _ => types.array(ret),
            },
            _ => Ty::OPEN,
        },
        MethodResult::Receiver => receiver,
        MethodResult::Flat => {
            let zero_depth = matches!(arg0_expr, Some(Expr::Number(n)) if *n <= 0.0)
                || matches!(arg0_expr, Some(Expr::Integer(n)) if *n <= 0);
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(inner) if !zero_depth => {
                    let (inner_def, states) = types.strip_nullish(inner);
                    let elem = match types.kind(inner_def) {
                        TyKind::Array(e) => e,
                        _ => inner_def,
                    };
                    let with_states = types.with_absences(elem, states);
                    types.array(with_states)
                }
                _ => stripped,
            }
        }
        MethodResult::Element | MethodResult::At => {
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(elem) => types.with_absences(elem, Absence::Undefined),
                TyKind::Tuple(span) => {
                    let items: Vec<Ty> = types.tuple_slice(span).to_vec();
                    let u = types.union(&items);
                    types.with_absences(u, Absence::Undefined)
                }
                TyKind::String if rule == MethodResult::At => {
                    types.with_absences(Ty::STRING, Absence::Undefined)
                }
                _ => Ty::OPEN,
            }
        }
        MethodResult::Reduced => {
            if let Some(ret) = callback_result
                && !types.is_open(ret)
                && ret != Ty::VOID
            {
                return ret;
            }
            if let Some(init_ty) = arg0
                && !types.is_open(init_ty)
            {
                return init_ty;
            }
            let (stripped, _) = types.strip_nullish(receiver);
            match types.kind(stripped) {
                TyKind::Array(elem) => elem,
                _ => Ty::OPEN,
            }
        }
        MethodResult::Void => Ty::VOID,
        MethodResult::String => Ty::STRING,
        MethodResult::Strings => types.array(Ty::STRING),
        MethodResult::NullableStrings => {
            let opt_str = types.with_absences(Ty::STRING, Absence::Undefined);
            let arr = types.array(opt_str);
            types.with_absences(arr, Absence::Null)
        }
        MethodResult::Number => Ty::NUMBER,
        MethodResult::Boolean => Ty::BOOLEAN,
        MethodResult::OptionalNumber => types.with_absences(Ty::NUMBER, Absence::Undefined),
        MethodResult::NullableString => types.with_absences(Ty::STRING, Absence::Null),
        MethodResult::Native(kind) => native_ty(ctx, types, kind),
        MethodResult::Nullable(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.with_absences(inner, Absence::Null)
        }
        MethodResult::Optional(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.with_absences(inner, Absence::Undefined)
        }
        MethodResult::Promise(kind) => {
            let inner = native_ty(ctx, types, kind);
            types.promise(inner)
        }
        MethodResult::PromiseVoid => types.promise(Ty::VOID),
        MethodResult::PromiseJson => types.promise(Ty::DYNAMIC),
        MethodResult::StreamReader => native_ty(
            ctx,
            types,
            if arg0_expr.is_some() {
                Kind::ByobReader
            } else {
                Kind::Reader
            },
        ),
        MethodResult::IteratorKeys => {
            iterator_view(ctx, types, receiver, IteratorMethod::Keys).unwrap_or(Ty::OPEN)
        }
        MethodResult::IteratorValues => {
            iterator_view(ctx, types, receiver, IteratorMethod::Values).unwrap_or(Ty::OPEN)
        }
        MethodResult::IteratorEntries => {
            iterator_view(ctx, types, receiver, IteratorMethod::Entries).unwrap_or(Ty::OPEN)
        }
        MethodResult::Lookup => {
            let (stripped, _) = types.strip_nullish(receiver);
            if let TyKind::Generic { base, args } = types.kind(stripped)
                && let HirType::Named(base_name) = base.hir(ctx)
                && matches!(base_name.as_str(), "Map" | "ReadonlyMap")
                && let [_, val] = types.generic_slice(args)
            {
                let val = *val;
                types.with_absences(val, Absence::Undefined)
            } else {
                Ty::OPEN
            }
        }
        MethodResult::Duration => duration_ty(types),
    }
}

pub(crate) fn native_ty(ctx: &Context, types: &mut TypeTable, kind: Kind) -> Ty {
    match kind {
        Kind::String => Ty::STRING,
        Kind::Number => Ty::NUMBER,
        Kind::Boolean => Ty::BOOLEAN,
        Kind::BigInt => Ty::BIGINT,
        other => types.named(ctx, other.label()),
    }
}

fn duration_ty(types: &mut TypeTable) -> Ty {
    let mut props = [
        ("blank", Ty::BOOLEAN),
        ("days", Ty::NUMBER),
        ("months", Ty::NUMBER),
        ("sign", Ty::NUMBER),
        ("weeks", Ty::NUMBER),
        ("years", Ty::NUMBER),
    ]
    .map(|(name, ty)| ObjectProp {
        name: types.intern_name(name),
        ty,
        optional: false,
        readonly: false,
    });
    types.object(&mut props, None)
}

/// Infer the result type of a binary operator from its already-typed operands.
pub(crate) fn binary_result(
    ctx: &Context,
    types: &mut TypeTable,
    op: BinaryOp,
    left: Ty,
    right: Ty,
) -> Ty {
    match op {
        BinaryOp::Add => {
            if is_string_ty(ctx, types, left) || is_string_ty(ctx, types, right) {
                return Ty::STRING;
            }
            if left == Ty::BOTTOM || right == Ty::BOTTOM {
                return Ty::BOTTOM;
            }
            if left == Ty::BIGINT && right == Ty::BIGINT {
                return Ty::BIGINT;
            }
            if is_mixed_string_union(ctx, types, left) || is_mixed_string_union(ctx, types, right) {
                return types.union(&[Ty::NUMBER, Ty::STRING]);
            }
            if is_numeric_operand(types, left) && is_numeric_operand(types, right) {
                return Ty::NUMBER;
            }
            Ty::NUMBER
        }
        BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Mod
        | BinaryOp::Pow
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::UShr => {
            if left == Ty::BIGINT && right == Ty::BIGINT {
                return Ty::BIGINT;
            }
            Ty::NUMBER
        }
    }
}

pub(crate) fn unary_result(op: UnaryOp, operand: Ty) -> Ty {
    match op {
        UnaryOp::Not => Ty::BOOLEAN,
        UnaryOp::Neg | UnaryOp::Pos | UnaryOp::BitNot => {
            if operand == Ty::BIGINT {
                Ty::BIGINT
            } else {
                Ty::NUMBER
            }
        }
    }
}

fn is_string_ty(ctx: &Context, types: &TypeTable, ty: Ty) -> bool {
    match types.kind(ty) {
        TyKind::String => true,
        TyKind::Leaf(id) => matches!(id.hir(ctx), HirType::StringLiteral(_)),
        _ => false,
    }
}

fn is_numeric_atom(ty: Ty) -> bool {
    matches!(
        ty,
        Ty::NUMBER | Ty::INT32 | Ty::BOOLEAN | Ty::NULL | Ty::VOID
    )
}

fn is_numeric_operand(types: &TypeTable, ty: Ty) -> bool {
    match types.kind(ty) {
        TyKind::Union(span) => types.union_slice(span).iter().all(|&m| is_numeric_atom(m)),
        _ => is_numeric_atom(ty),
    }
}

fn is_mixed_string_union(ctx: &Context, types: &TypeTable, ty: Ty) -> bool {
    let TyKind::Union(span) = types.kind(ty) else {
        return false;
    };
    types.union_slice(span).iter().any(|&m| {
        !is_numeric_atom(m)
            && (is_string_ty(ctx, types, m)
                || !matches!(types.kind(m), TyKind::Bottom | TyKind::Open))
    })
}

/// Resolve the type of a `new Map(...)` or `new Set(...)` constructor preserved in metadata.
pub(crate) fn constructor_type<'h>(
    ctx: &Context,
    types: &mut TypeTable,
    ctor: Constructor<'h>,
    eval: &mut impl FnMut(&'h Expr, &mut TypeTable) -> Ty,
) -> Ty {
    match ctor {
        Constructor::Map {
            types: explicit,
            init,
        } => {
            let base = shapes::TypeId::from_owned(ctx, HirType::Named("Map".to_owned()));
            if let Some((k, v)) = explicit {
                let k = types.intern_hir(ctx, k, true);
                let v = types.intern_hir(ctx, v, true);
                return types.generic(base, &[k, v]);
            }
            if let Some(init) = init {
                if let Expr::Array(entries, _) = init
                    && let Some(Expr::Array(pair, _)) = entries.first()
                    && let [key, value] = pair.as_slice()
                {
                    let k = eval(key, types);
                    let v = eval(value, types);
                    return types.generic(base, &[k, v]);
                }
                let init_ty = eval(init, types);
                let (stripped, _) = types.strip_nullish(init_ty);
                if let TyKind::Array(elem) = types.kind(stripped)
                    && let TyKind::Tuple(span) = types.kind(elem)
                    && let [k, v] = types.tuple_slice(span)
                {
                    let (k, v) = (*k, *v);
                    return types.generic(base, &[k, v]);
                }
            }
            Ty::OPEN
        }
        Constructor::Set { element, init } => {
            let base = shapes::TypeId::from_owned(ctx, HirType::Named("Set".to_owned()));
            if let Some(elem) = element {
                let elem = types.intern_hir(ctx, elem, true);
                return types.generic(base, &[elem]);
            }
            if let Some(init) = init {
                let init_ty = eval(init, types);
                if let Some(elem) = iterated_element(ctx, types, init_ty) {
                    return types.generic(base, &[elem]);
                }
            }
            Ty::OPEN
        }
    }
}

/// Classify a leaf HIR expression directly into [`Ty`] without recursing via `perry_hir`.
pub(crate) fn leaf_expr_type(ctx: &Context, types: &mut TypeTable, expr: &Expr) -> Option<Ty> {
    Some(match expr {
        Expr::Undefined | Expr::Void(_) => Ty::VOID,
        Expr::Null => Ty::NULL,
        Expr::Bool(_)
        | Expr::Compare { .. }
        | Expr::InstanceOf { .. }
        | Expr::In { .. }
        | Expr::Delete(_)
        | Expr::ObjectIs(_, _)
        | Expr::ObjectHasOwn(_, _)
        | Expr::ArrayIsArray(_)
        | Expr::ArrayIncludes { .. }
        | Expr::ArraySome { .. }
        | Expr::ArrayEvery { .. }
        | Expr::MapHas { .. }
        | Expr::MapDelete { .. }
        | Expr::SetHas { .. }
        | Expr::SetDelete { .. }
        | Expr::ThisProvenClass(_)
        | Expr::IsNaN(_)
        | Expr::IsUndefinedOrBareNan(_)
        | Expr::IsFinite(_)
        | Expr::NumberIsNaN(_)
        | Expr::NumberIsFinite(_)
        | Expr::NumberIsInteger(_)
        | Expr::NumberIsSafeInteger(_)
        | Expr::BooleanCoerce(_)
        | Expr::TextDecoderFatal(_)
        | Expr::TextDecoderIgnoreBom(_)
        | Expr::UrlCanParse(_)
        | Expr::UrlCanParseWithBase { .. }
        | Expr::UrlSearchParamsHas { .. } => Ty::BOOLEAN,

        Expr::Integer(_)
        | Expr::Number(_)
        | Expr::Update { .. }
        | Expr::PropertyUpdate { .. }
        | Expr::IndexUpdate { .. }
        | Expr::NumberCoerce(_)
        | Expr::MapSize(_)
        | Expr::SetSize(_)
        | Expr::ArrayPush { .. }
        | Expr::ArrayPushSpread { .. }
        | Expr::ArrayUnshift { .. }
        | Expr::ArrayIndexOf { .. }
        | Expr::ArrayLastIndexOf { .. }
        | Expr::ArrayFindIndex { .. }
        | Expr::ArrayFindLastIndex { .. }
        | Expr::PerformanceNow
        | Expr::DateNow
        | Expr::DateGetTime(_)
        | Expr::DateGetFullYear(_)
        | Expr::DateGetMonth(_)
        | Expr::DateGetDate(_)
        | Expr::DateGetDay(_)
        | Expr::DateGetHours(_)
        | Expr::DateGetMinutes(_)
        | Expr::DateGetSeconds(_)
        | Expr::DateGetMilliseconds(_)
        | Expr::DateParse(_)
        | Expr::DateUtc(_)
        | Expr::DateGetUtcDay(_)
        | Expr::DateGetUtcFullYear(_)
        | Expr::DateGetUtcMonth(_)
        | Expr::DateGetUtcDate(_)
        | Expr::DateGetUtcHours(_)
        | Expr::DateGetUtcMinutes(_)
        | Expr::DateGetUtcSeconds(_)
        | Expr::DateGetUtcMilliseconds(_)
        | Expr::DateSetUtcFullYear { .. }
        | Expr::DateSetUtcMonth { .. }
        | Expr::DateSetUtcDate { .. }
        | Expr::DateSetUtcHours { .. }
        | Expr::DateSetUtcMinutes { .. }
        | Expr::DateSetUtcSeconds { .. }
        | Expr::DateSetUtcMilliseconds { .. }
        | Expr::DateSetFullYear { .. }
        | Expr::DateSetMonth { .. }
        | Expr::DateSetDate { .. }
        | Expr::DateSetHours { .. }
        | Expr::DateSetMinutes { .. }
        | Expr::DateSetSeconds { .. }
        | Expr::DateSetMilliseconds { .. }
        | Expr::DateSetTime { .. }
        | Expr::DateValueOf(_)
        | Expr::DateGetTimezoneOffset(_)
        | Expr::MathRandom
        | Expr::MathFloor(_)
        | Expr::MathCeil(_)
        | Expr::MathRound(_)
        | Expr::MathTrunc(_)
        | Expr::MathSign(_)
        | Expr::MathAbs(_)
        | Expr::MathSqrt(_)
        | Expr::MathLog(_)
        | Expr::MathLog2(_)
        | Expr::MathLog10(_)
        | Expr::MathLog1p(_)
        | Expr::MathClz32(_)
        | Expr::MathSin(_)
        | Expr::MathCos(_)
        | Expr::MathTan(_)
        | Expr::MathAsin(_)
        | Expr::MathAcos(_)
        | Expr::MathAtan(_)
        | Expr::MathAtan2(_, _)
        | Expr::MathCbrt(_)
        | Expr::MathFround(_)
        | Expr::MathF16round(_)
        | Expr::MathExpm1(_)
        | Expr::MathHypot(_)
        | Expr::MathSinh(_)
        | Expr::MathCosh(_)
        | Expr::MathTanh(_)
        | Expr::MathAsinh(_)
        | Expr::MathAcosh(_)
        | Expr::MathAtanh(_)
        | Expr::MathExp(_)
        | Expr::MathMin(_)
        | Expr::MathMax(_)
        | Expr::MathPow(_, _)
        | Expr::MathImul(_, _)
        | Expr::MathMinSpread(_)
        | Expr::MathMaxSpread(_)
        | Expr::ParseInt { .. }
        | Expr::ParseFloat(_)
        | Expr::Uint8ArrayLength(_)
        | Expr::BufferByteLength { .. }
        | Expr::BufferLength(_) => Ty::NUMBER,

        // Reading a Uint8Array element can be out-of-bounds (`undefined`) as lowering models.
        Expr::Uint8ArrayGet { .. } | Expr::BufferIndexGet { .. } => {
            types.with_absences(Ty::NUMBER, Absence::Undefined)
        }

        Expr::BigInt(_) | Expr::BigIntCoerce(_) => Ty::BIGINT,

        Expr::String(_)
        | Expr::WtfString(_)
        | Expr::TypeOf(_)
        | Expr::StringCoerce(_)
        | Expr::TemplateStringCoerce(_)
        | Expr::ArrayJoin { .. }
        | Expr::JsonStringify(_)
        | Expr::JsonStringifyPretty { .. }
        | Expr::JsonStringifyFull(..)
        | Expr::CryptoRandomUUID
        | Expr::StringReplace { .. }
        | Expr::ErrorMessage(_)
        | Expr::EncodeURI(_)
        | Expr::DecodeURI(_)
        | Expr::EncodeURIComponent(_)
        | Expr::DecodeURIComponent(_)
        | Expr::Atob(_)
        | Expr::Btoa(_)
        | Expr::TextDecoderEncoding(_)
        | Expr::TextDecoderDecode { .. }
        | Expr::UrlGetHref(_)
        | Expr::UrlGetPathname(_)
        | Expr::UrlGetProtocol(_)
        | Expr::UrlGetHost(_)
        | Expr::UrlGetHostname(_)
        | Expr::UrlGetPort(_)
        | Expr::UrlGetSearch(_)
        | Expr::UrlGetHash(_)
        | Expr::UrlGetOrigin(_)
        | Expr::UrlInstanceToString(_)
        | Expr::UrlInstanceToJSON(_)
        | Expr::UrlSearchParamsToString(_)
        | Expr::StringFromCharCode(_)
        | Expr::StringFromCharCodeSpread(_)
        | Expr::StringFromCodePoint(_)
        | Expr::DateToISOString(_)
        | Expr::DateToString(_)
        | Expr::DateToDateString(_)
        | Expr::DateToTimeString(_)
        | Expr::DateToUTCString(_)
        | Expr::DateToLocaleString(_)
        | Expr::DateToLocaleDateString(_)
        | Expr::DateToLocaleTimeString(_) => Ty::STRING,

        Expr::DateToJSON(_) => types.with_absences(Ty::STRING, Absence::Null),
        Expr::StringCodePointAt { .. } => types.with_absences(Ty::NUMBER, Absence::Undefined),
        Expr::StringAt { .. } => types.with_absences(Ty::STRING, Absence::Undefined),
        Expr::EnvGet(_) | Expr::EnvGetDynamic(_) => {
            types.with_absences(Ty::STRING, Absence::Undefined)
        }
        Expr::UrlParse(_) | Expr::UrlParseWithBase { .. } => {
            let url = types.named(ctx, "URL");
            types.with_absences(url, Absence::Null)
        }
        Expr::UrlSearchParamsGet { .. } => types.with_absences(Ty::STRING, Absence::Null),

        Expr::ObjectKeys(_)
        | Expr::ForInKeys(_)
        | Expr::StringSplit(_, _)
        | Expr::UrlSearchParamsGetAll { .. }
        | Expr::UrlSearchParamsKeys(_)
        | Expr::UrlSearchParamsValues(_)
        | Expr::ProcessArgv => types.array(Ty::STRING),

        Expr::UrlSearchParamsEntries(_) => {
            let pair = types.tuple(&[Ty::STRING, Ty::STRING]);
            types.array(pair)
        }

        Expr::ArrayForEach { .. } | Expr::MapClear(_) | Expr::SetClear(_) => Ty::VOID,

        Expr::SymbolNew(_) | Expr::SymbolFor(_) => Ty::SYMBOL,
        Expr::RegExp { .. } | Expr::RegExpDynamic { .. } => types.named(ctx, "RegExp"),
        Expr::DateNew(_) => types.named(ctx, "Date"),
        Expr::ErrorNew(_) | Expr::ErrorNewWithCause { .. } | Expr::ErrorNewWithOptions { .. } => {
            types.named(ctx, "Error")
        }
        Expr::TypeErrorNew(_) => types.named(ctx, "TypeError"),
        Expr::RangeErrorNew(_) => types.named(ctx, "RangeError"),
        Expr::ReferenceErrorNew(_) => types.named(ctx, "ReferenceError"),
        Expr::SyntaxErrorNew(_) => types.named(ctx, "SyntaxError"),
        Expr::UrlNew { .. } => types.named(ctx, "URL"),
        Expr::UrlSearchParamsNew(_) | Expr::UrlGetSearchParams(_) => {
            types.named(ctx, "URLSearchParams")
        }
        Expr::TextEncoderNew => types.named(ctx, "TextEncoder"),
        Expr::TextDecoderNew { .. } => types.named(ctx, "TextDecoder"),
        Expr::Uint8ArrayNew(_)
        | Expr::Uint8ArrayFrom(_)
        | Expr::BufferAlloc { .. }
        | Expr::BufferAllocUnsafe(_)
        | Expr::BufferFrom { .. }
        | Expr::BufferFromArrayBuffer { .. }
        | Expr::BufferConcat(_)
        | Expr::BufferConcatWithLength { .. }
        | Expr::BufferSlice { .. }
        | Expr::BufferFill { .. }
        | Expr::TextEncoderEncode(_)
        | Expr::CryptoRandomBytes(_) => types.named(ctx, "Uint8Array"),

        Expr::JsonParse(_) | Expr::JsonParseWithReviver(..) | Expr::JsonParseReviver { .. } => {
            Ty::DYNAMIC
        }
        Expr::JsonParseTyped { ty, .. } => {
            let parsed = types.intern_hir(ctx, ty, true);
            if parsed == Ty::OPEN {
                Ty::DYNAMIC
            } else {
                parsed
            }
        }
        _ => return None,
    })
}

/// Property lookup on built-in, structural object, or declared nominal types.
pub(crate) fn property_type(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    source: Ty,
    property: &str,
) -> Option<Ty> {
    let (stripped, _) = types.strip_nullish(source);
    match types.kind(stripped) {
        TyKind::String if property == "length" => Some(Ty::NUMBER),
        TyKind::Array(_) | TyKind::Tuple(_) => match property {
            "length" | "index" => Some(Ty::NUMBER),
            "input" => Some(Ty::STRING),
            "groups" => Some(Ty::DYNAMIC),
            _ => None,
        },
        TyKind::Object { props, index } => {
            let target_name = types.intern_name(property);
            let found = types
                .object_slice(props)
                .iter()
                .find(|p| p.name == target_name)
                .copied();
            if let Some(prop) = found {
                return Some(if prop.optional {
                    types.with_absences(prop.ty, Absence::Undefined)
                } else {
                    prop.ty
                });
            }
            index.map(|idx| types.with_absences(idx, Absence::Undefined))
        }
        TyKind::Generic { base, args } => {
            let HirType::Named(base_name) = base.hir(ctx) else {
                return None;
            };
            if matches!(
                base_name.as_str(),
                "Map" | "ReadonlyMap" | "Set" | "ReadonlySet"
            ) && property == "size"
            {
                return Some(Ty::NUMBER);
            }
            if base_name == SETTLED && !decls.is_user_type(base_name) {
                let slice: Vec<Ty> = types.generic_slice(args).to_vec();
                return match (property, slice.as_slice()) {
                    ("status", _) => Some(Ty::STRING),
                    ("value", [val]) => Some(types.with_absences(*val, Absence::Undefined)),
                    ("reason", _) => {
                        let err = types.named(ctx, "Error");
                        Some(types.with_absences(err, Absence::Undefined))
                    }
                    _ => None,
                };
            }
            let arg_hirs: Vec<HirType> = types
                .generic_slice(args)
                .iter()
                .map(|&t| types.materialize_hir(ctx, t, &|_| None))
                .collect();
            if let Some(prop_hir) = decls.property_type(ctx, base_name, &arg_hirs, property) {
                let ty = types.intern_hir(ctx, &prop_hir, true);
                return Some(normalize_named_integer(ctx, decls, types, ty));
            }
            let generic_hir = HirType::Generic {
                base: base_name.clone(),
                type_args: arg_hirs,
            };
            if let Some((_, val_hir)) = decls.dict_entry_types(ctx, &generic_hir) {
                let val_ty = types.intern_hir(ctx, &val_hir, true);
                return Some(types.with_absences(val_ty, Absence::Undefined));
            }
            lookup_method_result(ctx, types, stripped, property, true).map(|rule| {
                apply_method_result(ctx, types, rule, MethodResultArgs::receiver_only(stripped))
            })
        }
        TyKind::Leaf(id) => {
            let HirType::Named(base_name) = id.hir(ctx) else {
                if matches!(id.hir(ctx), HirType::StringLiteral(_)) && property == "length" {
                    return Some(Ty::NUMBER);
                }
                return None;
            };
            if Kind::builtin(base_name) == Some(Kind::Exception)
                && matches!(property, "message" | "name")
            {
                return Some(Ty::STRING);
            }
            match (base_name.as_str(), property) {
                ("Temporal.PlainDate", "year" | "month" | "day" | "dayOfWeek")
                | (
                    "Temporal.PlainDateTime",
                    "year" | "month" | "day" | "dayOfWeek" | "hour" | "minute" | "second"
                    | "millisecond" | "microsecond" | "nanosecond",
                )
                | ("Temporal.Instant", "epochMilliseconds") => return Some(Ty::NUMBER),
                ("Temporal.PlainDate" | "Temporal.PlainDateTime", "calendarId") => {
                    return Some(Ty::STRING);
                }
                _ => {}
            }
            if let Some(prop_hir) = decls.property_type(ctx, base_name, &[], property) {
                let ty = types.intern_hir(ctx, &prop_hir, true);
                return Some(normalize_named_integer(ctx, decls, types, ty));
            }
            if let Some(HirType::Object(obj)) = decls.resolve_foreign_type(base_name)
                && let Some(prop) = obj.properties.get(property)
            {
                let ty = types.intern_hir(ctx, &prop.ty, true);
                let ty = if prop.optional {
                    types.with_absences(ty, Absence::Undefined)
                } else {
                    ty
                };
                return Some(normalize_named_integer(ctx, decls, types, ty));
            }
            if let Some((_, val_hir)) =
                decls.dict_entry_types(ctx, &HirType::Named(base_name.clone()))
            {
                let val_ty = types.intern_hir(ctx, &val_hir, true);
                return Some(types.with_absences(val_ty, Absence::Undefined));
            }
            if let Some(rule) = Method::named_property(base_name, property) {
                return Some(apply_method_result(
                    ctx,
                    types,
                    rule,
                    MethodResultArgs::receiver_only(stripped),
                ));
            }
            lookup_method_result(ctx, types, stripped, property, true).map(|rule| {
                apply_method_result(ctx, types, rule, MethodResultArgs::receiver_only(stripped))
            })
        }
        TyKind::Union(span) => {
            // Property read on a finite value union (e.g. `Uint8Array | string`: `.length`).
            let members: Vec<Ty> = types.union_slice(span).to_vec();
            let mut results = Vec::with_capacity(members.len());
            for member in members {
                let ty = property_type(ctx, decls, types, member, property)?;
                results.push(ty);
            }
            Some(types.union(&results))
        }
        _ => None,
    }
}

/// Normalize a property name lookup inside `Object` shapes.
pub(crate) fn object_property(types: &TypeTable, ty: Ty, name: NameId) -> Option<(Ty, bool)> {
    let TyKind::Object { props, .. } = types.kind(ty) else {
        return None;
    };
    types
        .object_slice(props)
        .iter()
        .find(|p| p.name == name)
        .map(|p| (p.ty, p.optional))
}

/// Evaluate `object[index]` on a `Ty`.
pub(crate) fn index_type(
    ctx: &Context,
    decls: &Declarations<'_>,
    types: &mut TypeTable,
    object: Ty,
    index: &Expr,
) -> Option<Ty> {
    let (stripped, _) = types.strip_nullish(object);
    match types.kind(stripped) {
        TyKind::Tuple(span) => {
            let pos = match index {
                Expr::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
                Expr::Integer(n) => usize::try_from(*n).ok(),
                _ => None,
            };
            let slice = types.tuple_slice(span);
            match pos {
                Some(i) => Some(slice.get(i).copied().unwrap_or(Ty::VOID)),
                None => {
                    let items: Vec<Ty> = slice.to_vec();
                    Some(types.union(&items))
                }
            }
        }
        TyKind::Array(elem) => Some(elem),
        TyKind::String => Some(Ty::STRING),
        TyKind::Leaf(id) => match id.hir(ctx) {
            HirType::Named(name) if name == "Uint8Array" => {
                Some(types.with_absences(Ty::NUMBER, Absence::Undefined))
            }
            HirType::Named(base) => {
                if let Some((_, val_hir)) =
                    decls.dict_entry_types(ctx, &HirType::Named(base.clone()))
                {
                    let val_ty = types.intern_hir(ctx, &val_hir, true);
                    return Some(types.with_absences(val_ty, Absence::Undefined));
                }
                if let Some(HirType::Array(elem)) = decls.resolve_foreign_type(base) {
                    return Some(types.intern_hir(ctx, elem, true));
                }
                None
            }
            _ => None,
        },
        TyKind::Object { props, index: idx } => {
            if let Expr::String(key) = index {
                let name = types.intern_name(key);
                if let Some((ty, opt)) = object_property(types, stripped, name) {
                    return Some(if opt {
                        types.with_absences(ty, Absence::Undefined)
                    } else {
                        ty
                    });
                }
            }
            if let Some(idx_ty) = idx {
                return Some(types.with_absences(idx_ty, Absence::Undefined));
            }
            let items: Vec<Ty> = types.object_slice(props).iter().map(|p| p.ty).collect();
            (!items.is_empty()).then(|| types.union(&items))
        }
        TyKind::Generic { .. } => {
            if let Some(elem) = iterated_element(ctx, types, stripped) {
                return Some(elem);
            }
            let hir = types.materialize_hir(ctx, stripped, &|_| None);
            decls.dict_entry_types(ctx, &hir).map(|(_, val_hir)| {
                let val_ty = types.intern_hir(ctx, &val_hir, true);
                types.with_absences(val_ty, Absence::Undefined)
            })
        }
        _ => None,
    }
}

/// Evaluate static `Promise.allSettled`, `Map.groupBy`, `performance.now`, and `Temporal.*` calls.
pub(crate) fn static_global_method<'h>(
    ctx: &Context,
    types: &mut TypeTable,
    class: &str,
    method: &str,
    args: &'h [Expr],
    eval: &mut impl FnMut(&'h Expr, &mut TypeTable) -> Ty,
) -> Option<Ty> {
    match (class, method) {
        ("performance", "now") => Some(Ty::NUMBER),
        ("Promise", "allSettled") => {
            let [items] = args else {
                return Some(Ty::OPEN);
            };
            let src = eval(items, types);
            let elem = iterated_element(ctx, types, src).unwrap_or(Ty::OPEN);
            let val = match types.kind(elem) {
                TyKind::Promise(inner) => inner,
                _ => elem,
            };
            let settled_base = shapes::TypeId::from_owned(ctx, HirType::Named(SETTLED.to_owned()));
            let settled = types.generic(settled_base, &[val]);
            let arr = types.array(settled);
            Some(types.promise(arr))
        }
        ("Promise", "all") => {
            let [items] = args else {
                return Some(Ty::OPEN);
            };
            let src = eval(items, types);
            let elem = iterated_element(ctx, types, src).unwrap_or(Ty::OPEN);
            let val = match types.kind(elem) {
                TyKind::Promise(inner) => inner,
                _ => elem,
            };
            let arr = types.array(val);
            Some(types.promise(arr))
        }
        ("Promise", "race") => {
            let [items] = args else {
                return Some(Ty::OPEN);
            };
            let src = eval(items, types);
            let elem = iterated_element(ctx, types, src).unwrap_or(Ty::OPEN);
            let val = match types.kind(elem) {
                TyKind::Promise(inner) => inner,
                _ => elem,
            };
            Some(types.promise(val))
        }
        ("Promise", "resolve") => {
            let val = args.first().map_or(Ty::VOID, |a| eval(a, types));
            Some(match types.kind(val) {
                TyKind::Promise(_) => val,
                _ => types.promise(val),
            })
        }
        ("Promise", "reject") => Some(types.promise(Ty::VOID)),
        _ => None,
    }
}

pub(crate) fn temporal_static_method(
    ctx: &Context,
    types: &mut TypeTable,
    class: &str,
    method: &str,
) -> Option<Ty> {
    Some(match (class, method) {
        ("Instant", "from" | "fromEpochMilliseconds") | ("Now", "instant") => {
            types.named(ctx, "Temporal.Instant")
        }
        ("PlainDate", "from") | ("Now", "plainDateISO") => types.named(ctx, "Temporal.PlainDate"),
        ("PlainDate", "compare") => Ty::NUMBER,
        ("PlainDateTime", "from") | ("Now", "plainDateTimeISO") => {
            types.named(ctx, "Temporal.PlainDateTime")
        }
        ("Now", "timeZoneId") => Ty::STRING,
        _ => return None,
    })
}

/// Evaluate `LogicalOp` (`??`, `||`, `&&`) over `Ty` operands.
pub(crate) fn logical_result(types: &mut TypeTable, op: LogicalOp, left: Ty, right: Ty) -> Ty {
    match op {
        LogicalOp::Coalesce | LogicalOp::Or => {
            let (left_inner, _) = types.strip_nullish(left);
            let (right_inner, right_abs) = types.strip_nullish(right);
            let base = if !types.is_open(left_inner) && left_inner != Ty::VOID {
                if !types.is_open(right_inner)
                    && right_inner != Ty::VOID
                    && right_inner != left_inner
                {
                    types.join(left_inner, right_inner)
                } else {
                    left_inner
                }
            } else {
                right_inner
            };
            types.with_absences(base, right_abs)
        }
        LogicalOp::And => {
            let (left_inner, left_abs) = types.strip_nullish(left);
            let (right_inner, right_abs) = types.strip_nullish(right);
            let abs = Absence::join_optional(left_abs, right_abs);
            let base = if !types.is_open(right_inner) && right_inner != Ty::VOID {
                right_inner
            } else {
                left_inner
            };
            types.with_absences(base, abs)
        }
    }
}
