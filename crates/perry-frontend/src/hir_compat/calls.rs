//! Compiler-generated synthetic calls recognized during HIR lowering.
use super::*;

const TYPED: &str = "\0perry_typed";
const PENDING: &str = "\0perry_promise_pending";
const RESOLVE: &str = "\0perry_promise_resolve";
const REJECT: &str = "\0perry_promise_reject";
const TIMER: &str = "\0perry_timer";

pub(crate) fn is_source_extern(name: &str, offset: u32) -> bool {
    has_source_offset(offset) && !name.starts_with("js_") && !name.starts_with("__")
}

pub(crate) fn is_array_insertion(method: &str) -> bool {
    matches!(method, "push" | "push_single" | "unshift")
}

pub enum GuestCall<E, T> {
    Typed { value: E, ty: T },
    Pending(T),
    Resolve { promise: E, value: E },
    Reject { promise: E, reason: E },
    Timer { delay: E, signal: E },
}

impl GuestCall<Expr, HirType> {
    pub(crate) fn into_hir(self) -> Expr {
        let (name, args, type_args) = match self {
            Self::Typed { value, ty } => (TYPED, vec![value], vec![ty]),
            Self::Pending(settled) => (PENDING, vec![], vec![settled]),
            Self::Resolve { promise, value } => (RESOLVE, vec![promise, value], vec![]),
            Self::Reject { promise, reason } => (REJECT, vec![promise, reason], vec![]),
            Self::Timer { delay, signal } => (TIMER, vec![delay, signal], vec![]),
        };
        Expr::Call {
            callee: Box::new(Expr::ExternFuncRef {
                name: name.to_owned(),
                param_types: Vec::new(),
                return_type: HirType::Any,
            }),
            args,
            type_args,
            byte_offset: 0,
            byte_end: 0,
        }
    }
}

pub enum RuntimeCall<'a> {
    Guest(GuestCall<&'a Expr, &'a HirType>),
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

pub fn runtime_call<'a>(
    callee: &'a Expr,
    args: &'a [Expr],
    type_args: &'a [HirType],
) -> Option<RuntimeCall<'a>> {
    use perry_hir::tdz_check::{TDZ_CHECK, TDZ_CHECK_THEN};
    let Expr::ExternFuncRef { name, .. } = callee else {
        return None;
    };
    Some(match (name.as_str(), args) {
        (TYPED, [value]) => {
            let [ty] = type_args else {
                return None;
            };
            RuntimeCall::Guest(GuestCall::Typed { value, ty })
        }
        (PENDING, []) => {
            let [ty] = type_args else {
                return None;
            };
            RuntimeCall::Guest(GuestCall::Pending(ty))
        }
        (RESOLVE, [promise, value]) => RuntimeCall::Guest(GuestCall::Resolve { promise, value }),
        (REJECT, [promise, reason]) => RuntimeCall::Guest(GuestCall::Reject { promise, reason }),
        (TIMER, [delay, signal]) => RuntimeCall::Guest(GuestCall::Timer { delay, signal }),
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
