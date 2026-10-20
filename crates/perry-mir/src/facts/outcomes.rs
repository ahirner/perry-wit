use super::*;

/// Possible settlements; bottom also covers a recursive promise that never settles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Outcomes {
    #[default]
    Bottom,
    Fulfilled,
    Rejected,
    Either,
}

impl Outcomes {
    pub(super) fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Bottom, value) | (value, Self::Bottom) => value,
            (a, b) if a == b => a,
            _ => Self::Either,
        }
    }

    pub(super) fn may_reject(self) -> bool {
        matches!(self, Self::Rejected | Self::Either)
    }

    pub(super) fn unknown(ctx: &Context, mut ty: TypeHandle) -> Self {
        while let Some(inner) = crate::optional::optional_value(ctx, ty) {
            ty = inner;
        }
        if ty.deref(ctx).is::<PromiseType>() {
            Self::Either
        } else {
            Self::Bottom
        }
    }
}

impl State {
    pub(super) fn outcome(&self, value: Value) -> Outcomes {
        self.value(value).outcome
    }
}

impl ProgramFacts {
    pub fn may_reject(&self, function: &FuncOp, promise: Value) -> bool {
        let state = &self.states[self.generic[&function.get_operation()]];
        state.outcome(promise).may_reject()
    }
}
