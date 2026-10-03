//! Exception handling, unwinding, and finally cleanup for the WAFFLE SSA backend.
//!
//! Provides structured control-flow abstractions for `try`, `catch`, `finally`,
//! and `throw` statements without requiring post-emission bytecode patching
//! or runtime dispatch tables.

use perry_hir::types::LocalId;
use std::collections::BTreeMap;
use waffle::{Block, BlockTarget, FunctionBody, Operator, Terminator, Type, Value};

/// Exit reason passed to a finally handler block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitReason {
    /// Normal sequential fallthrough out of the protected block.
    Normal = 0,
    /// Explicit `return` statement out of the protected block.
    Return = 1,
    /// Explicit `throw` statement or propagated callee exception out of the protected block.
    Throw = 2,
}

impl ExitReason {
    pub(crate) fn tag(self) -> u32 {
        self as u32
    }
}

/// Destination for unwinding an exception (`throw`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum UnwindTarget<'a> {
    /// Catch block in the current or an enclosing scope.
    Catch {
        block: Block,
        param: Option<LocalId>,
        scope_locals: &'a [LocalId],
    },
    /// Finally block that must run before further unwinding.
    Finally {
        block: Block,
        scope_locals: &'a [LocalId],
    },
    /// No handlers remain in the function body; unwind through the function boundary.
    FunctionExit,
}

/// Destination for an explicit `return`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ReturnTarget<'a> {
    /// Finally block that must run before the return completes.
    Finally {
        block: Block,
        scope_locals: &'a [LocalId],
    },
    /// No finally blocks remain; return directly out of the function.
    FunctionExit,
}

/// An active `try` scope in the function lowerer.
#[derive(Debug, Clone)]
pub(crate) struct TryScope {
    /// Target block for `catch`, active only while lowering the `try` body.
    pub(crate) catch_target: Option<Block>,
    /// Optional local ID for the caught exception variable (e.g. `catch (e)`).
    pub(crate) catch_param: Option<LocalId>,
    /// Target block for `finally`, active while lowering both `try` and `catch` bodies.
    pub(crate) finally_target: Option<Block>,
    /// Tracked incoming local variables at the entry to the try/catch statement.
    pub(crate) scope_locals: Vec<LocalId>,
}

/// Manages nested exception handling and cleanup scopes during WAFFLE lowering.
#[derive(Debug, Default)]
pub(crate) struct UnwindContext {
    scopes: Vec<TryScope>,
}

impl UnwindContext {
    pub(crate) fn new() -> Self {
        Self { scopes: Vec::new() }
    }

    /// Pushes a new try scope.
    pub(crate) fn push_scope(&mut self, scope: TryScope) {
        self.scopes.push(scope);
    }

    /// Pops the innermost try scope.
    pub(crate) fn pop_scope(&mut self) -> Option<TryScope> {
        self.scopes.pop()
    }

    /// Clears the catch target in the innermost scope (called when entering a catch body).
    pub(crate) fn clear_catch_in_innermost(&mut self) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.catch_target = None;
            scope.catch_param = None;
        }
    }

    /// Finds the immediate destination for a `throw`, borrowing scope locals.
    pub(crate) fn target_for_throw(&self) -> UnwindTarget<'_> {
        for scope in self.scopes.iter().rev() {
            if let Some(catch_block) = scope.catch_target {
                return UnwindTarget::Catch {
                    block: catch_block,
                    param: scope.catch_param,
                    scope_locals: &scope.scope_locals,
                };
            }
            if let Some(finally_block) = scope.finally_target {
                return UnwindTarget::Finally {
                    block: finally_block,
                    scope_locals: &scope.scope_locals,
                };
            }
        }
        UnwindTarget::FunctionExit
    }

    /// Finds the immediate destination for an explicit `return`, borrowing scope locals.
    pub(crate) fn target_for_return(&self) -> ReturnTarget<'_> {
        for scope in self.scopes.iter().rev() {
            if let Some(finally_block) = scope.finally_target {
                return ReturnTarget::Finally {
                    block: finally_block,
                    scope_locals: &scope.scope_locals,
                };
            }
        }
        ReturnTarget::FunctionExit
    }
}

/// SSA blocks and parameter layout for a `try / catch / finally` statement.
pub(crate) struct TryClauseBlocks {
    pub(crate) join_block: Block,
    pub(crate) catch_block: Option<Block>,
    pub(crate) catch_param: Option<LocalId>,
    pub(crate) finally_block: Option<Block>,
    pub(crate) scope_locals: Vec<LocalId>,
}

impl TryClauseBlocks {
    /// Constructs the SSA blocks and block parameters for try, catch, finally, and join.
    pub(crate) fn build(
        body: &mut FunctionBody,
        locals: &BTreeMap<LocalId, Value>,
        has_catch: bool,
        catch_param: Option<LocalId>,
        has_finally: bool,
    ) -> Self {
        let scope_locals: Vec<LocalId> = locals.keys().copied().collect();
        let join_block = body.add_block();
        body.blocks[join_block].desc = "try-finally join".into();

        for &id in &scope_locals {
            let ty = body.values[locals[&id]]
                .ty(&body.type_pool)
                .expect("Local binding must have a valid type");
            body.add_blockparam(join_block, ty);
        }

        let finally_block = if has_finally {
            let fb = body.add_block();
            body.blocks[fb].desc = "finally entry".into();
            body.add_blockparam(fb, Type::I32); // exit_reason
            body.add_blockparam(fb, Type::F64); // payload
            for &id in &scope_locals {
                let ty = body.values[locals[&id]].ty(&body.type_pool).unwrap();
                body.add_blockparam(fb, ty);
            }
            Some(fb)
        } else {
            None
        };

        let catch_block = if has_catch {
            let cb = body.add_block();
            body.blocks[cb].desc = "catch entry".into();
            body.add_blockparam(cb, Type::F64); // exception payload
            for &id in &scope_locals {
                let ty = body.values[locals[&id]].ty(&body.type_pool).unwrap();
                body.add_blockparam(cb, ty);
            }
            Some(cb)
        } else {
            None
        };

        Self {
            join_block,
            catch_block,
            catch_param,
            finally_block,
            scope_locals,
        }
    }

    /// Emits normal exit transition from a try or catch body:
    /// branches to finally (with ExitReason::Normal, 0.0) if present, or to join_block.
    /// Returns true if join_block was branched to directly.
    pub(crate) fn emit_normal_transition(
        &self,
        body: &mut FunctionBody,
        from_block: Block,
        current_locals: &BTreeMap<LocalId, Value>,
    ) -> bool {
        if let Some(fb) = self.finally_block {
            let zero_reason = body.add_op(
                from_block,
                Operator::I32Const {
                    value: ExitReason::Normal.tag(),
                },
                &[],
                &[Type::I32],
            );
            let zero_payload = body.add_op(
                from_block,
                Operator::F64Const {
                    value: 0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            let mut args = vec![zero_reason, zero_payload];
            for id in &self.scope_locals {
                args.push(current_locals[id]);
            }
            body.set_terminator(
                from_block,
                Terminator::Br {
                    target: BlockTarget { block: fb, args },
                },
            );
            false
        } else {
            let args = self
                .scope_locals
                .iter()
                .map(|id| current_locals[id])
                .collect();
            body.set_terminator(
                from_block,
                Terminator::Br {
                    target: BlockTarget {
                        block: self.join_block,
                        args,
                    },
                },
            );
            true
        }
    }

    /// Rebuilds catch bindings without retaining locals declared in the try body.
    pub(crate) fn catch_environment(&self, body: &FunctionBody) -> BTreeMap<LocalId, Value> {
        let block = self.catch_block.expect("Catch block must exist");
        let mut locals = self.scope_environment(body, block, 1);
        if let Some(param_id) = self.catch_param {
            locals.insert(param_id, body.blocks[block].params[0].1);
        }
        locals
    }

    /// Rebuilds finally bindings and preserves its pending exit independently of locals.
    pub(crate) fn finally_environment(&self, body: &FunctionBody) -> FinallyEnvironment {
        let block = self.finally_block.expect("Finally block must exist");
        FinallyEnvironment {
            locals: self.scope_environment(body, block, 2),
            exit_reason: body.blocks[block].params[0].1,
            payload: body.blocks[block].params[1].1,
        }
    }

    /// Rebuilds the outer environment after normal or handled completion.
    pub(crate) fn join_environment(&self, body: &FunctionBody) -> BTreeMap<LocalId, Value> {
        self.scope_environment(body, self.join_block, 0)
    }

    /// Selects only the bindings carried into this scope by block parameters.
    fn scope_environment(
        &self,
        body: &FunctionBody,
        block: Block,
        offset: usize,
    ) -> BTreeMap<LocalId, Value> {
        self.scope_locals
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, body.blocks[block].params[index + offset].1))
            .collect()
    }
}

/// Bindings and pending completion at a finally clause's entry.
pub(crate) struct FinallyEnvironment {
    pub(crate) locals: BTreeMap<LocalId, Value>,
    pub(crate) exit_reason: Value,
    pub(crate) payload: Value,
}

/// Dispatches the exit reason from a finally block:
/// - ExitReason::Normal (0) -> branches to join_block with current scope locals
/// - ExitReason::Return (1) -> branches to `on_return` block
/// - ExitReason::Throw (2)  -> branches to `on_throw` block
///
/// Returns `(on_return_block, on_throw_block)`.
pub(crate) fn emit_finally_dispatcher(
    body: &mut FunctionBody,
    finally_exit_block: Block,
    exit_reason: Value,
    join_block: Block,
    scope_locals: &[LocalId],
    current_locals: &BTreeMap<LocalId, Value>,
) -> (Block, Block) {
    let on_normal = body.add_block();
    body.blocks[on_normal].desc = "finally dispatch normal".into();
    let on_not_normal = body.add_block();
    body.blocks[on_not_normal].desc = "finally dispatch non-normal".into();

    let is_normal = body.add_op(
        finally_exit_block,
        Operator::I32Eqz,
        &[exit_reason],
        &[Type::I32],
    );
    body.set_terminator(
        finally_exit_block,
        Terminator::CondBr {
            cond: is_normal,
            if_true: BlockTarget {
                block: on_normal,
                args: vec![],
            },
            if_false: BlockTarget {
                block: on_not_normal,
                args: vec![],
            },
        },
    );

    // on_normal branches to join_block
    let join_args = scope_locals.iter().map(|id| current_locals[id]).collect();
    body.set_terminator(
        on_normal,
        Terminator::Br {
            target: BlockTarget {
                block: join_block,
                args: join_args,
            },
        },
    );

    // on_not_normal: check return vs throw
    let on_return = body.add_block();
    body.blocks[on_return].desc = "finally dispatch return".into();
    let on_throw = body.add_block();
    body.blocks[on_throw].desc = "finally dispatch throw".into();

    let one = body.add_op(
        on_not_normal,
        Operator::I32Const { value: 1 },
        &[],
        &[Type::I32],
    );
    let is_return = body.add_op(
        on_not_normal,
        Operator::I32Eq,
        &[exit_reason, one],
        &[Type::I32],
    );
    body.set_terminator(
        on_not_normal,
        Terminator::CondBr {
            cond: is_return,
            if_true: BlockTarget {
                block: on_return,
                args: vec![],
            },
            if_false: BlockTarget {
                block: on_throw,
                args: vec![],
            },
        },
    );

    (on_return, on_throw)
}

/// Routes an explicit return or return exit from finally:
/// - If enclosed by a finally block, branches to finally with `(ExitReason::Return, payload, ...locals)` and returns `false`.
/// - If at function exit, returns `true` indicating the caller should emit the terminal function return.
pub(crate) fn route_return(
    body: &mut FunctionBody,
    block: Block,
    unwind_ctx: &UnwindContext,
    current_locals: &BTreeMap<LocalId, Value>,
    payload: Value,
) -> bool {
    match unwind_ctx.target_for_return() {
        ReturnTarget::Finally {
            block: finally_block,
            scope_locals,
        } => {
            let reason_val = body.add_op(
                block,
                Operator::I32Const {
                    value: ExitReason::Return.tag(),
                },
                &[],
                &[Type::I32],
            );
            let mut args = vec![reason_val, payload];
            for id in scope_locals {
                args.push(current_locals[id]);
            }
            body.set_terminator(
                block,
                Terminator::Br {
                    target: BlockTarget {
                        block: finally_block,
                        args,
                    },
                },
            );
            false
        }
        ReturnTarget::FunctionExit => true,
    }
}

/// Routes an exception (`throw` or propagated callee exception):
/// - If caught, branches to catch with `(payload, ...locals)` and returns `false`.
/// - If enclosed by finally, branches to finally with `(ExitReason::Throw, payload, ...locals)` and returns `false`.
/// - If at function exit, returns `true` indicating the caller should emit the terminal function throw.
pub(crate) fn route_throw(
    body: &mut FunctionBody,
    block: Block,
    unwind_ctx: &UnwindContext,
    current_locals: &BTreeMap<LocalId, Value>,
    err_val_f64: Value,
) -> bool {
    match unwind_ctx.target_for_throw() {
        UnwindTarget::Catch {
            block: catch_block,
            param: _param,
            scope_locals,
        } => {
            let mut args = vec![err_val_f64];
            for id in scope_locals {
                args.push(current_locals[id]);
            }
            body.set_terminator(
                block,
                Terminator::Br {
                    target: BlockTarget {
                        block: catch_block,
                        args,
                    },
                },
            );
            false
        }
        UnwindTarget::Finally {
            block: finally_block,
            scope_locals,
        } => {
            let reason_val = body.add_op(
                block,
                Operator::I32Const {
                    value: ExitReason::Throw.tag(),
                },
                &[],
                &[Type::I32],
            );
            let mut args = vec![reason_val, err_val_f64];
            for id in scope_locals {
                args.push(current_locals[id]);
            }
            body.set_terminator(
                block,
                Terminator::Br {
                    target: BlockTarget {
                        block: finally_block,
                        args,
                    },
                },
            );
            false
        }
        UnwindTarget::FunctionExit => true,
    }
}
