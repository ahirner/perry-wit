//! Source schemas to the existing MIR representation contracts.
use super::*;

pub(super) fn integer_mir_carrier(ctx: &Context, width: u32) -> TypeHandle {
    if width == 64 {
        perry_mir::BigIntType::get(ctx).into()
    } else {
        FP64Type::get(ctx).into()
    }
}

impl Shapes<'_> {
    pub(crate) fn value_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        let ty = self
            .specialization
            .types
            .borrow_mut()
            .intern_hir(ctx, ty, true);
        Ok(self.resolved_binding(ctx, ty)?.carrier)
    }

    /// Return the MIR value type that a promise of `ty` settles with, mapping `void` to `undefined`.
    pub(crate) fn settled_type(&self, ctx: &Context, ty: &HirType) -> Result<TypeHandle> {
        match ty {
            HirType::Void => {
                let number = FP64Type::get(ctx).into();
                Ok(OptionalType::get(ctx, number).into())
            }
            ty => self.value_type(ctx, ty),
        }
    }

    pub(crate) fn integer_type(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Option<TypedHandle<IntegerType>> {
        let HirType::Named(name) = ty else {
            return None;
        };
        let (width, signedness) = self.decls.integer_spec(name)?;
        Some(IntegerType::get(ctx, width, signedness))
    }

    pub(super) fn foreign_value_type(
        &self,
        ctx: &Context,
        entry: &crate::wit::Foreign,
    ) -> Result<TypeHandle> {
        match entry {
            crate::wit::Foreign::Type(value) => value.mir_type(ctx, false),
            crate::wit::Foreign::Input(value) => value.mir_type(ctx, true),
            crate::wit::Foreign::Function(_) => {
                bail_diag!(
                    codes::TYPE_MISMATCH,
                    "foreign function cannot be used as a value type"
                )
            }
        }
    }
}
