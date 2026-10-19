//! Physical storage used by the canonical ABI conversion traversal.
use super::*;

pub(super) enum Canonical<'a> {
    // Flat inputs are reversed for consumption; outputs are appended in ABI order.
    Flat(&'a mut Vec<Value>),
    Memory(Value, u32),
    JoinedInput(&'a [(Value, Flat)], &'a mut usize),
    JoinedOutput(&'a mut Vec<Value>, &'a [Flat]),
}

impl Canonical<'_> {
    pub(super) fn at(&mut self, offset: u32) -> Canonical<'_> {
        match self {
            Self::Flat(values) => Canonical::Flat(values),
            Self::Memory(address, base) => Canonical::Memory(*address, *base + offset),
            Self::JoinedInput(values, index) => Canonical::JoinedInput(values, index),
            Self::JoinedOutput(values, layout) => Canonical::JoinedOutput(values, layout),
        }
    }

    pub(super) fn read(
        &mut self,
        builder: &mut FunctionLowering<'_, '_>,
        ctx: &mut Context,
        kind: MemKind,
        offset: u32,
    ) -> Result<Value> {
        match self {
            Self::Flat(values) => values.pop().ok_or_else(|| anyhow!("too few flat values")),
            Self::JoinedInput(values, index) => {
                let (value, from) = values
                    .get(**index)
                    .copied()
                    .ok_or_else(|| anyhow!("too few joined flat values"))?;
                **index += 1;
                Ok(builder.coerce_flat(ctx, value, from, Flat::for_memory(kind)))
            }
            Self::JoinedOutput(..) => bail!("cannot read an output payload"),
            Self::Memory(address, base) => Ok(builder.load(ctx, kind, *address, *base + offset)),
        }
    }

    pub(super) fn write(
        &mut self,
        builder: &mut FunctionLowering<'_, '_>,
        ctx: &mut Context,
        kind: MemKind,
        offset: u32,
        value: Value,
    ) -> Result<()> {
        match self {
            Self::Flat(values) => values.push(value),
            Self::JoinedOutput(values, layout) => {
                let to = layout
                    .get(values.len())
                    .copied()
                    .ok_or_else(|| anyhow!("too many joined flat values"))?;
                values.push(builder.coerce_flat(ctx, value, Flat::for_memory(kind), to));
            }
            Self::JoinedInput(..) => bail!("cannot write an input payload"),
            Self::Memory(address, base) => {
                builder.store(ctx, kind, *address, *base + offset, value)
            }
        }
        Ok(())
    }
}

impl Flat {
    pub(super) fn for_memory(kind: MemKind) -> Self {
        match kind {
            MemKind::F32 => Self::F32,
            MemKind::F64 => Self::F64,
            MemKind::I64 | MemKind::I64U32 => Self::I64,
            _ => Self::I32,
        }
    }
    pub(super) fn mem_kind(self) -> MemKind {
        match self {
            Self::I32 => MemKind::I32,
            Self::I64 => MemKind::I64,
            Self::F32 => MemKind::F32,
            Self::F64 => MemKind::F64,
        }
    }
}
