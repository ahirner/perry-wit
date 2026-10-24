//! Compiler-generated synthetic calls recognized during HIR lowering.
use super::*;

pub enum RuntimeCall<'a> {
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

pub fn runtime_call<'a>(callee: &'a Expr, args: &'a [Expr]) -> Option<RuntimeCall<'a>> {
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
