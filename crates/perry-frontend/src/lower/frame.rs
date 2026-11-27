//! Function-local state and lexical jump destinations during MIR lowering.
use super::*;

pub(super) struct Frame<'s> {
    pub(super) owner: BodyOwner,
    pub(super) this_slot: Option<Value>,
    pub(super) controls: Vec<Control<'s>>,
    pub(super) return_to: Option<Ptr<BasicBlock>>,
    pub(super) cursors: hir_compat::CursorPositions,
    pub(super) rest_excluded_keys: &'s [String],
}

impl<'s> Frame<'s> {
    pub(super) fn new(owner: BodyOwner, this_slot: Option<Value>) -> Self {
        Self {
            owner,
            this_slot,
            controls: Vec::new(),
            return_to: None,
            cursors: Default::default(),
            rest_excluded_keys: &[],
        }
    }
    pub(super) fn jump_target(
        &self,
        jump: Jump,
        label: Option<&str>,
    ) -> Result<(usize, Ptr<BasicBlock>)> {
        self.controls
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, control)| {
                control
                    .target(jump, label)
                    .map(|target| (index + 1, target))
            })
            .ok_or_else(|| match label {
                Some(label) => diag!(
                    codes::PARSE_ERROR,
                    "{} {label} has no enclosing target",
                    match jump {
                        Jump::Break => "break",
                        Jump::Continue => "continue",
                    }
                ),
                None => diag!(codes::PARSE_ERROR, "break/continue has no enclosing target"),
            })
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn with_inline_scope<T>(
        &mut self,
        ctx: &mut Context,
        owner: FuncId,
        this_slot: Option<Value>,
        lower: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let callee = Frame::new(BodyOwner::Function(owner), this_slot);
        let caller = std::mem::replace(&mut self.frame, callee);
        // The callee sees captured locals and caller guards. Writes can invalidate inherited guards.
        let bindings = self.locals.scope();
        let guards = self.variant_guards.depth();
        let result = lower(self, ctx);
        self.locals.restore(bindings);
        self.variant_guards.truncate(guards);
        self.frame = caller;
        result
    }

    pub(super) fn with_handler<T>(
        &mut self,
        ctx: &mut Context,
        handler: Option<Handler>,
        finally: Option<&'s [Stmt]>,
        lower: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let outer = std::mem::replace(&mut self.handler, handler);
        let depth = self.frame.controls.len();
        if let Some(body) = finally {
            self.frame.controls.push(Control::Finally {
                body,
                handler: outer,
            });
        }
        let result = lower(self, ctx);
        self.frame.controls.truncate(depth);
        self.handler = outer;
        result
    }
}

pub(super) enum Control<'s> {
    Finally {
        body: &'s [Stmt],
        handler: Option<Handler>,
    },
    Loop {
        label: Option<&'s str>,
        continue_to: Ptr<BasicBlock>,
        break_to: Ptr<BasicBlock>,
    },
    Switch {
        label: Option<&'s str>,
        break_to: Ptr<BasicBlock>,
    },
    Labeled {
        label: &'s str,
        break_to: Ptr<BasicBlock>,
    },
}

#[derive(Clone, Copy)]
pub(super) enum Jump {
    Break,
    Continue,
}

impl Control<'_> {
    fn target(&self, jump: Jump, label: Option<&str>) -> Option<Ptr<BasicBlock>> {
        let (own_label, break_to, continue_to) = match self {
            Self::Finally { .. } => return None,
            Self::Loop {
                label,
                break_to,
                continue_to,
            } => (*label, *break_to, Some(*continue_to)),
            Self::Switch { label, break_to } => (*label, *break_to, None),
            Self::Labeled { label, break_to } => (Some(*label), *break_to, None),
        };
        if let Some(label) = label {
            if own_label != Some(label) {
                return None;
            }
        } else if matches!(self, Self::Labeled { .. }) {
            return None;
        }
        match jump {
            Jump::Break => Some(break_to),
            Jump::Continue => continue_to,
        }
    }
}

/// The exceptional edge carries the thrown status into this block.
pub(super) type Handler = Ptr<BasicBlock>;
