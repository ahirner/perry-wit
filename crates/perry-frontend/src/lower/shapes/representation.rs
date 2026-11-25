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
    pub(in crate::lower) fn annotation(&self, ctx: &Context, ty: &HirType) -> Ty {
        self.specialization
            .types
            .borrow_mut()
            .intern_hir(ctx, ty, true)
    }

    pub(in crate::lower) fn source_binding(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Result<BindingType> {
        self.resolved_binding(ctx, self.annotation(ctx, ty))
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
