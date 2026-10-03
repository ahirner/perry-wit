//! Exception handling, unwinding, and finally cleanup for the WAFFLE SSA backend.
//!
//! Provides structured control-flow abstractions for `try`, `catch`, `finally`,
//! and `throw` statements without requiring post-emission bytecode patching
//! or runtime dispatch tables.

use perry_hir::types::LocalId;
use waffle::Block;

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
#[derive(Debug, Clone)]
pub(crate) enum UnwindTarget {
    /// Catch block in the current or an enclosing scope.
    Catch {
        block: Block,
        param: Option<LocalId>,
        scope_locals: Vec<LocalId>,
    },
    /// Finally block that must run before further unwinding.
    Finally {
        block: Block,
        scope_locals: Vec<LocalId>,
    },
    /// No handlers remain in the function body; unwind through the function boundary.
    FunctionExit,
}

/// Destination for an explicit `return`.
#[derive(Debug, Clone)]
pub(crate) enum ReturnTarget {
    /// Finally block that must run before the return completes.
    Finally {
        block: Block,
        scope_locals: Vec<LocalId>,
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

    /// Finds the immediate destination for a `throw`.
    pub(crate) fn target_for_throw(&self) -> UnwindTarget {
        for scope in self.scopes.iter().rev() {
            if let Some(catch_block) = scope.catch_target {
                return UnwindTarget::Catch {
                    block: catch_block,
                    param: scope.catch_param,
                    scope_locals: scope.scope_locals.clone(),
                };
            }
            if let Some(finally_block) = scope.finally_target {
                return UnwindTarget::Finally {
                    block: finally_block,
                    scope_locals: scope.scope_locals.clone(),
                };
            }
        }
        UnwindTarget::FunctionExit
    }

    /// Finds the immediate destination for an explicit `return`.
    pub(crate) fn target_for_return(&self) -> ReturnTarget {
        for scope in self.scopes.iter().rev() {
            if let Some(finally_block) = scope.finally_target {
                return ReturnTarget::Finally {
                    block: finally_block,
                    scope_locals: scope.scope_locals.clone(),
                };
            }
        }
        ReturnTarget::FunctionExit
    }
}
