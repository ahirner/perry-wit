//! Guest Promise continuations own suspended state until a microtask resumes it.

use std::collections::VecDeque;

use crate::callbacks::guest_callback_invoke;
use crate::nanbox::{nanbox_pointer, TAG_FALSE, TAG_TRUE, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

#[derive(Clone, Debug)]
pub(crate) enum Promise {
    Pending(Vec<Reaction>),
    Adopting(Vec<Reaction>),
    Settled { value: i64, rejected: bool },
}

#[derive(Clone, Debug)]
pub(crate) struct Resolver {
    pub(crate) result: i64,
    rejected: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Reaction {
    Step { callback: i64, result: i64 },
    Adopt { result: i64 },
    Forward { result: i64 },
}

#[derive(Clone, Copy)]
struct Activation {
    callback: i64,
    result: i64,
}

#[derive(Clone, Copy)]
struct Job {
    reaction: Reaction,
    value: i64,
    rejected: bool,
}

/// Runtime scratch slots are only read within the current step's guest frame.
#[derive(Default)]
pub(crate) struct Promises {
    jobs: VecDeque<Job>,
    activation: Option<Activation>,
    iter_value: i64,
    iter_done: bool,
}

impl Promise {
    pub(crate) fn trace(&self, roots: &mut Vec<i64>) {
        match self {
            Self::Pending(reactions) | Self::Adopting(reactions) => {
                for reaction in reactions {
                    reaction.trace(roots);
                }
            }
            Self::Settled { value, .. } => roots.push(*value),
        }
    }
}

impl Reaction {
    fn trace(&self, roots: &mut Vec<i64>) {
        match self {
            Self::Step { callback, result } => roots.extend([*callback, *result]),
            Self::Adopt { result } | Self::Forward { result } => roots.push(*result),
        }
    }
}

impl Promises {
    pub(crate) fn trace(&self, roots: &mut Vec<i64>) {
        for job in &self.jobs {
            job.reaction.trace(roots);
            roots.push(job.value);
        }
        if let Some(activation) = self.activation {
            roots.extend([activation.callback, activation.result, self.iter_value]);
        }
    }

    pub(crate) fn cancel_all(&mut self) {
        self.jobs.clear();
        self.activation = None;
        self.iter_value = TAG_UNDEFINED as i64;
        self.iter_done = false;
    }
}

pub(crate) fn dispatch_call(name: &str, arguments: &[i64]) -> Option<i64> {
    if matches!(name, "then" | "catch" | "finally")
        && arguments.first().is_some_and(|value| {
            matches!(get_state().get_handle(*value), Some(JsHandle::Promise(_)))
        })
    {
        get_state().current_exception =
            Some("TypeError: Promise instance methods are not supported yet".into());
        return Some(TAG_UNDEFINED as i64);
    }
    if !name.starts_with("async_") {
        return None;
    }
    let arguments = if arguments.first() == Some(&(TAG_UNDEFINED as i64)) {
        &arguments[1..]
    } else {
        arguments
    };
    let argument = |index| {
        arguments
            .get(index)
            .copied()
            .unwrap_or(TAG_UNDEFINED as i64)
    };
    let activation = get_state().promises.activation;
    if matches!(name, "async_step_chain" | "async_resolve" | "async_reject") && activation.is_none()
    {
        get_state().current_exception =
            Some("Error: Async bridge requires an active guest task".into());
        return Some(TAG_UNDEFINED as i64);
    }
    Some(match name {
        "async_promise_new" => construct(argument(0)),
        "async_cell_new" => {
            nanbox_pointer(get_state().alloc_handle(JsHandle::Cell(TAG_UNDEFINED as i64)))
        }
        "async_cell_get" => cell_get(argument(0)),
        "async_cell_set" => {
            cell_set(argument(0), argument(1));
            argument(1)
        }
        "async_cell_update" => {
            let old = get_state().to_number(cell_get(argument(0)));
            let next = old + get_state().to_number(argument(1));
            cell_set(argument(0), next.to_bits() as i64);
            (if argument(2) == TAG_TRUE as i64 {
                next
            } else {
                old
            })
            .to_bits() as i64
        }
        "async_iter_set" => {
            let promises = &mut get_state().promises;
            promises.iter_value = argument(0);
            promises.iter_done = argument(1) == TAG_TRUE as i64;
            TAG_UNDEFINED as i64
        }
        "async_iter_value" => get_state().promises.iter_value,
        "async_iter_done" => {
            (if get_state().promises.iter_done {
                TAG_TRUE
            } else {
                TAG_FALSE
            }) as i64
        }
        "async_current_step" => get_state()
            .promises
            .activation
            .map(|activation| activation.callback)
            .unwrap_or(TAG_UNDEFINED as i64),
        "async_first_call" => {
            let result = new_pending();
            invoke_step(argument(0), TAG_UNDEFINED as i64, false, result);
            result
        }
        "async_step_chain" => {
            let result = activation.unwrap().result;
            attach(
                argument(0),
                Reaction::Step {
                    callback: argument(1),
                    result,
                },
            );
            result
        }
        "async_resolve" | "async_reject" => {
            let result = activation.unwrap().result;
            settle(result, argument(0), name == "async_reject");
            result
        }
        _ => return None,
    })
}

fn cell_get(handle: i64) -> i64 {
    match get_state().get_handle(handle) {
        Some(JsHandle::Cell(value)) => *value,
        _ => TAG_UNDEFINED as i64,
    }
}

fn cell_set(handle: i64, value: i64) {
    if let Some(JsHandle::Cell(stored)) = get_state().get_handle_mut(handle) {
        *stored = value;
    }
}

fn new_pending() -> i64 {
    nanbox_pointer(get_state().alloc_handle(JsHandle::Promise(Promise::Pending(Vec::new()))))
}

/// The executor runs synchronously; its exception rejects only an unresolved Promise.
fn construct(executor: i64) -> i64 {
    if !matches!(
        get_state().get_handle(executor),
        Some(JsHandle::Closure(_) | JsHandle::PromiseResolver(_))
    ) {
        get_state().current_exception =
            Some("TypeError: Promise executor is not a function".into());
        return TAG_UNDEFINED as i64;
    }
    let result = new_pending();
    let state = get_state();
    let resolve = nanbox_pointer(state.alloc_handle(JsHandle::PromiseResolver(Resolver {
        result,
        rejected: false,
    })));
    let reject = nanbox_pointer(state.alloc_handle(JsHandle::PromiseResolver(Resolver {
        result,
        rejected: true,
    })));
    let arguments = nanbox_pointer(state.alloc_handle(JsHandle::Array(vec![resolve, reject])));
    guest_callback_invoke(executor, arguments);
    if let Some(error) = get_state().current_exception.take() {
        let value = get_state().alloc_string(&error);
        settle(result, value, true);
    }
    result
}

/// Builtin resolver functions use the callback bridge without a Rust table cast.
pub(crate) fn invoke_resolver(handle: i64, arguments: i64) -> i64 {
    let state = get_state();
    let (Some(JsHandle::PromiseResolver(resolver)), Some(JsHandle::Array(arguments))) =
        (state.get_handle(handle), state.get_handle(arguments))
    else {
        return crate::callbacks::guest_callback_invalid();
    };
    let (result, rejected) = (resolver.result, resolver.rejected);
    let value = arguments.first().copied().unwrap_or(TAG_UNDEFINED as i64);
    settle(result, value, rejected);
    TAG_UNDEFINED as i64
}

fn attach(value: i64, reaction: Reaction) {
    let state = get_state();
    let (value, rejected) = match state.get_handle_mut(value) {
        Some(JsHandle::Promise(Promise::Pending(reactions) | Promise::Adopting(reactions))) => {
            reactions.push(reaction);
            return;
        }
        Some(JsHandle::Promise(Promise::Settled { value, rejected })) => (*value, *rejected),
        _ => (value, false),
    };
    state.promises.jobs.push_back(Job {
        reaction,
        value,
        rejected,
    });
}

fn settle(result: i64, value: i64, rejected: bool) {
    if !matches!(
        get_state().get_handle(result),
        Some(JsHandle::Promise(Promise::Pending(_)))
    ) {
        return;
    }
    if !rejected && matches!(get_state().get_handle(value), Some(JsHandle::Promise(_))) {
        if result == value {
            let error = get_state().alloc_string("TypeError: Chaining cycle detected for promise");
            settle(result, error, true);
        } else {
            if let Some(JsHandle::Promise(promise)) = get_state().get_handle_mut(result) {
                let Promise::Pending(reactions) =
                    std::mem::replace(promise, Promise::Adopting(Vec::new()))
                else {
                    return;
                };
                *promise = Promise::Adopting(reactions);
            }
            get_state().promises.jobs.push_back(Job {
                reaction: Reaction::Adopt { result },
                value,
                rejected: false,
            });
        }
        return;
    }
    complete(result, value, rejected);
}

fn complete(result: i64, value: i64, rejected: bool) {
    let state = get_state();
    if let Some(JsHandle::Promise(promise @ (Promise::Pending(_) | Promise::Adopting(_)))) =
        state.get_handle_mut(result)
    {
        let (Promise::Pending(reactions) | Promise::Adopting(reactions)) =
            std::mem::replace(promise, Promise::Settled { value, rejected })
        else {
            return;
        };
        state
            .promises
            .jobs
            .extend(reactions.into_iter().map(|reaction| Job {
                reaction,
                value,
                rejected,
            }));
    }
}

fn invoke_step(callback: i64, value: i64, rejected: bool, result: i64) {
    let state = get_state();
    let previous = state
        .promises
        .activation
        .replace(Activation { callback, result });
    let arguments = nanbox_pointer(state.alloc_handle(JsHandle::Array(vec![
        value,
        (if rejected { TAG_TRUE } else { TAG_FALSE }) as i64,
    ])));
    guest_callback_invoke(callback, arguments);
    let state = get_state();
    state.promises.activation = previous;
    state.promises.iter_value = TAG_UNDEFINED as i64;
    if let Some(error) = state.current_exception.take() {
        let value = state.alloc_string(&error);
        settle(result, value, true);
    }
}

/// Runs one microtask; the ABI can reclaim temporaries after its guest frame returns.
#[no_mangle]
pub(crate) extern "C" fn guest_async_step() -> i32 {
    let Some(job) = get_state().promises.jobs.pop_front() else {
        return 0;
    };
    match job.reaction {
        Reaction::Step { callback, result } => {
            invoke_step(callback, job.value, job.rejected, result);
        }
        Reaction::Adopt { result } => attach(job.value, Reaction::Forward { result }),
        Reaction::Forward { result } => complete(result, job.value, job.rejected),
    }
    1
}

/// Reports whether an exported task still needs a producer to settle its result.
#[no_mangle]
pub(crate) extern "C" fn guest_async_pending(value: i64) -> i32 {
    i32::from(matches!(
        get_state().get_handle(value),
        Some(JsHandle::Promise(
            Promise::Pending(_) | Promise::Adopting(_)
        ))
    ))
}

/// Projects a settled guest task Promise onto the synchronous Preview 2 boundary.
#[no_mangle]
pub(crate) extern "C" fn guest_async_result(value: i64) -> i64 {
    let state = get_state();
    match state.get_handle(value) {
        Some(JsHandle::Promise(Promise::Settled {
            value,
            rejected: false,
        })) => *value,
        Some(JsHandle::Promise(Promise::Settled {
            value,
            rejected: true,
        })) => {
            state.current_exception = Some(state.get_string(*value));
            TAG_UNDEFINED as i64
        }
        Some(JsHandle::Promise(Promise::Pending(_) | Promise::Adopting(_))) => {
            state.current_exception =
                Some("Error: Guest Promise has no runnable continuation".into());
            TAG_UNDEFINED as i64
        }
        _ => value,
    }
}

/// Legacy synchronous await keeps its enclosing guest frame live while jobs run.
pub(crate) fn await_value(value: i64) -> i64 {
    while matches!(
        get_state().get_handle(value),
        Some(JsHandle::Promise(
            Promise::Pending(_) | Promise::Adopting(_)
        ))
    ) {
        if guest_async_step() == 0 {
            break;
        }
    }
    guest_async_result(value)
}
