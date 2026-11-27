//! Compiler-generated synthetic calls recognized during HIR lowering.
use super::*;

pub(crate) fn is_source_extern(name: &str, offset: u32) -> bool {
    has_source_offset(offset) && !name.starts_with("js_") && !name.starts_with("__")
}

pub(crate) fn is_array_insertion(method: &str) -> bool {
    matches!(method, "push" | "push_single" | "unshift")
}

pub(crate) fn class_receiver(object: &Expr) -> Option<&str> {
    match object {
        Expr::ClassRef(name) | Expr::ExternFuncRef { name, .. } => Some(name),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MemberKey<'h> {
    Named(&'h str, u32),
    Computed(&'h Expr, u32),
}

impl<'h> MemberKey<'h> {
    pub(crate) fn offset(self) -> u32 {
        match self {
            Self::Named(_, offset) | Self::Computed(_, offset) => offset,
        }
    }
    pub(crate) fn name(self) -> Option<&'h str> {
        match self {
            Self::Named(name, _) => Some(name),
            Self::Computed(..) => None,
        }
    }
}

pub(crate) fn member(callee: &Expr) -> Option<(&Expr, MemberKey<'_>)> {
    match callee {
        Expr::PropertyGet {
            object,
            property,
            byte_offset,
            ..
        } => Some((object, MemberKey::Named(property, *byte_offset))),
        Expr::IndexGet {
            object,
            index,
            byte_offset,
            ..
        } => Some((object, MemberKey::Computed(index, *byte_offset))),
        _ => None,
    }
}

pub(crate) fn member_assignment(expr: &Expr) -> Option<(&Expr, MemberKey<'_>, &Expr)> {
    use MemberKey::{Computed, Named};
    match expr {
        Expr::PropertySet {
            object,
            property,
            value,
            byte_offset,
            ..
        } => Some((object, Named(property, *byte_offset), value)),
        Expr::IndexSet {
            object,
            index,
            value,
            byte_offset,
            ..
        } => Some((object, Computed(index, *byte_offset), value)),
        Expr::PutValueSet {
            target,
            key,
            value,
            byte_offset,
            ..
        } => {
            let key = match key.as_ref() {
                Expr::String(name) => Named(name, *byte_offset),
                expr => Computed(expr, *byte_offset),
            };
            Some((target, key, value))
        }
        _ => None,
    }
}

pub(crate) fn map_group_by(expr: &Expr) -> Result<Option<(&Expr, &Expr)>> {
    match expr {
        Expr::MapGroupBy { items, key_fn } => Ok(Some((items, key_fn))),
        Expr::Call { callee, args, .. }
            if let Some((class, MemberKey::Named(name, _))) = member(callee)
                && method(name).name() == "groupBy"
                && let Some((global, MemberKey::Named("Map", _))) = member(class)
                && matches!(global, Expr::GlobalGet(_)) =>
        {
            let [items, key_fn] = args.as_slice() else {
                perry_wit_diagnostics::bail_diag!(
                    codes::TYPE_MISMATCH,
                    "Map.groupBy requires (items, callbackFn)"
                );
            };
            Ok(Some((items, key_fn)))
        }
        _ => Ok(None),
    }
}

pub(crate) fn call_arguments(expr: &Expr) -> Option<(&[Expr], u32)> {
    let args = match expr {
        Expr::New { args, .. } | Expr::Call { args, .. } | Expr::StaticMethodCall { args, .. } => {
            args
        }
        _ => return None,
    };
    Some((args, expr.byte_offset()?))
}

pub(crate) enum RuntimeCall<'a> {
    Unresolved(&'a str),
    CheckInitialized {
        local: LocalId,
        value: Option<&'a Expr>,
    },
    CollectionEpoch,
    CollectionCursor {
        collection: &'a Expr,
        index: &'a Expr,
    },
    ExitCode(&'a [Expr]),
}

pub(crate) fn runtime_call<'a>(callee: &'a Expr, args: &'a [Expr]) -> Option<RuntimeCall<'a>> {
    use perry_hir::tdz_check::{TDZ_CHECK, TDZ_CHECK_THEN};
    let Expr::ExternFuncRef { name, .. } = callee else {
        return None;
    };
    Some(match (name.as_str(), args) {
        (
            "js_global_get_or_throw_unresolved"
            | "js_global_assign_existing_or_throw"
            | "js_global_update",
            [Expr::String(name), ..],
        ) => RuntimeCall::Unresolved(name),
        (TDZ_CHECK, [Expr::LocalGet(local), Expr::String(_)]) => RuntimeCall::CheckInitialized {
            local: *local,
            value: None,
        },
        (TDZ_CHECK_THEN, [Expr::LocalGet(local), Expr::String(_), value]) => {
            RuntimeCall::CheckInitialized {
                local: *local,
                value: Some(value),
            }
        }
        ("js_map_compaction_epoch" | "js_set_compaction_epoch", _) => RuntimeCall::CollectionEpoch,
        ("js_map_cursor_next" | "js_set_cursor_next", [collection, index, _]) => {
            RuntimeCall::CollectionCursor { collection, index }
        }
        ("js_process_exit_code_set", args) => RuntimeCall::ExitCode(args),
        _ => return None,
    })
}
