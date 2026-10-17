//! Multi-carrier lowering for MIR values whose runtime representation uses a register pair.
use crate::{function::FunctionLowering, repr::wasm_type};
use anyhow::{Result, anyhow, ensure};
use perry_mir::NumericType;
use pliron::{
    builtin::types::{IntegerType, Signedness},
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};

#[derive(Clone, Copy)]
pub(crate) enum Carriers<T> {
    Scalar(T),
    Numeric { tag: T, payload: T },
}

impl<T> From<T> for Carriers<T> {
    fn from(value: T) -> Self {
        Self::Scalar(value)
    }
}

impl<T: Copy> Carriers<T> {
    pub(crate) fn scalar(self) -> Result<T> {
        match self {
            Self::Scalar(value) => Ok(value),
            Self::Numeric { .. } => Err(anyhow!(
                "numeric alternatives require both register carriers"
            )),
        }
    }

    /// The `(tag, payload)` pair of a numeric alternative.
    pub(crate) fn numeric(self) -> Result<(T, T)> {
        match self {
            Self::Numeric { tag, payload } => Ok((tag, payload)),
            Self::Scalar(_) => Err(anyhow!("numeric alternatives require a tagged carrier")),
        }
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = T> {
        let (first, second) = match self {
            Self::Scalar(value) => (value, None),
            Self::Numeric { tag, payload } => (tag, Some(payload)),
        };
        std::iter::once(first).chain(second)
    }

    pub(crate) fn try_map<U>(self, mut f: impl FnMut(T) -> Result<U>) -> Result<Carriers<U>> {
        Ok(match self {
            Self::Scalar(value) => Carriers::Scalar(f(value)?),
            Self::Numeric { tag, payload } => Carriers::Numeric {
                tag: f(tag)?,
                payload: f(payload)?,
            },
        })
    }

    pub(crate) fn map<U>(self, mut f: impl FnMut(T) -> U) -> Carriers<U> {
        match self {
            Self::Scalar(value) => Carriers::Scalar(f(value)),
            Self::Numeric { tag, payload } => Carriers::Numeric {
                tag: f(tag),
                payload: f(payload),
            },
        }
    }

    pub(crate) fn zip<U: Copy>(self, other: Carriers<U>) -> Result<Carriers<(T, U)>> {
        match (self, other) {
            (Self::Scalar(a), Carriers::Scalar(b)) => Ok(Carriers::Scalar((a, b))),
            (
                Self::Numeric {
                    tag: at,
                    payload: ap,
                },
                Carriers::Numeric {
                    tag: bt,
                    payload: bp,
                },
            ) => Ok(Carriers::Numeric {
                tag: (at, bt),
                payload: (ap, bp),
            }),
            _ => Err(anyhow!("carrier shape mismatch")),
        }
    }
}

pub(crate) fn is_numeric_type(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::repr::optional_inner(ctx, ty).unwrap_or(ty);
    inner.deref(ctx).is::<NumericType>()
}

pub(crate) fn carriers(ctx: &Context, ty: TypeHandle) -> Result<Carriers<TypeHandle>> {
    if is_numeric_type(ctx, ty) {
        Ok(Carriers::Numeric {
            tag: IntegerType::get(ctx, 32, Signedness::Signless).into(),
            payload: IntegerType::get(ctx, 64, Signedness::Signless).into(),
        })
    } else {
        Ok(Carriers::Scalar(wasm_type(ctx, ty)?))
    }
}

pub(crate) fn signature(
    ctx: &Context,
    types: impl IntoIterator<Item = TypeHandle>,
) -> Result<Vec<TypeHandle>> {
    let mut result = Vec::new();
    for ty in types {
        result.extend(carriers(ctx, ty)?.iter());
    }
    Ok(result)
}

impl FunctionLowering<'_, '_> {
    pub(crate) fn bind_values(
        &mut self,
        ctx: &Context,
        logical: impl IntoIterator<Item = Value>,
        physical: impl IntoIterator<Item = Value>,
    ) -> Result<()> {
        let mut physical = physical.into_iter();
        for value in logical {
            let parts = carriers(ctx, value.get_type(ctx))?.try_map(|ty| {
                let carrier = physical
                    .next()
                    .ok_or_else(|| anyhow!("missing value carrier"))?;
                ensure!(carrier.get_type(ctx) == ty, "value carrier type mismatch");
                Ok(carrier)
            })?;
            self.values.insert(value, parts);
        }
        ensure!(physical.next().is_none(), "unexpected value carrier");
        Ok(())
    }

    pub(crate) fn flatten_operands(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        start: usize,
    ) -> Result<Vec<Value>> {
        let count = op.deref(ctx).get_num_operands();
        let mut result = Vec::new();
        for index in start..count {
            let value = op.deref(ctx).get_operand(index);
            result.extend(self.mapped_value(ctx, value)?.iter());
        }
        Ok(result)
    }
}
