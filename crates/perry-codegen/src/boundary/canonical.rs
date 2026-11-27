//! Physical storage used by the canonical ABI conversion traversal.
use super::*;
use perry_wit_diagnostics::Diagnostic;

pub(super) enum Canonical<F> {
    Flat(F),
    Memory(Value, u32),
}

impl<F: std::ops::DerefMut> Canonical<F> {
    pub(super) fn at(&mut self, offset: u32) -> Canonical<&mut F::Target> {
        match self {
            Self::Flat(values) => Canonical::Flat(values),
            Self::Memory(address, base) => Canonical::Memory(*address, *base + offset),
        }
    }
}

impl Canonical<&mut &[Value]> {
    pub(super) fn read(
        &mut self,
        builder: &mut FunctionLowering<'_, '_>,
        ctx: &mut Context,
        kind: MemKind,
        offset: u32,
    ) -> Result<Value> {
        match self {
            Self::Flat(values) => values
                .split_off_first()
                .copied()
                .ok_or_else(|| Diagnostic::internal("too few flat values")),
            Self::Memory(address, base) => Ok(builder.load(ctx, kind, *address, *base + offset)),
        }
    }
}

impl Canonical<&mut Vec<Value>> {
    pub(super) fn write(
        &mut self,
        builder: &mut FunctionLowering<'_, '_>,
        ctx: &mut Context,
        kind: MemKind,
        offset: u32,
        value: Value,
    ) {
        match self {
            Self::Flat(values) => values.push(value),
            Self::Memory(address, base) => {
                builder.store(ctx, kind, *address, *base + offset, value)
            }
        }
    }
}
