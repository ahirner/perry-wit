//! Source schemas to the existing MIR representation contracts.
use super::*;
use crate::engine::types::{FuncIdentity, Idx, func_tag};

pub(super) fn integer_mir_carrier(ctx: &Context, width: u32) -> TypeHandle {
    if width == 64 {
        perry_mir::BigIntType::get(ctx).into()
    } else {
        FP64Type::get(ctx).into()
    }
}

impl Shapes<'_> {
    pub(in crate::lower) fn source_binding(&self, ty: &HirType) -> Result<BindingType> {
        let ty = self.specialization.annotations.get(ty)?;
        self.resolved_binding(ty)
    }
}

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn foreign_value_type(
        &mut self,
        ctx: &Context,
        entry: &crate::wit::Foreign,
    ) -> Result<TypeHandle> {
        match entry {
            crate::wit::Foreign::Type(value) => value.mir_type(ctx, &mut self.records, false),
            crate::wit::Foreign::Input(value) => value.mir_type(ctx, &mut self.records, true),
            crate::wit::Foreign::Function(_) => {
                bail_diag!(
                    codes::TYPE_MISMATCH,
                    "foreign function cannot be used as a value type"
                )
            }
        }
    }
}

impl<T> Shapes<'_, T> {
    pub(in crate::lower) fn callable_environment(
        &self,
        function: FuncIdentity,
    ) -> Option<TypeHandle> {
        let layouts = &self.specialization.instances.environments;
        layouts.id(function).map(|id| layouts.record(id).into())
    }
}

impl Shapes<'_> {
    pub(in crate::lower) fn callable_type(&self, ty: Ty) -> Result<Option<TypeHandle>> {
        self.callables
            .contains(ty.index())
            .then(|| self.resolved_binding(ty).map(|binding| binding.carrier))
            .transpose()
    }
}

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn callable_type(&mut self, ctx: &Context, ty: Ty) -> Result<Option<TypeHandle>> {
        let types = &*self.types.source;
        let mut cases = Vec::new();
        let mut absent = None;
        let members = match types.kind(ty) {
            TyKind::Union(span) => types.ty_slice(span),
            _ => std::slice::from_ref(&ty),
        };
        for &member in members {
            let state = match types.kind(member) {
                TyKind::Func(identity) => {
                    cases.push((func_tag(identity), self.callable_environment(identity)));
                    continue;
                }
                TyKind::Void => perry_mir::Absence::Undefined,
                TyKind::Null => perry_mir::Absence::Null,
                _ => return Ok(None),
            };
            absent = perry_mir::Absence::join_optional(absent, Some(state));
        }
        let present = match cases.as_slice() {
            [] => return Ok(None),
            [(_, payload)] => {
                let mut fields = vec![("tag".into(), StringType::get(ctx).into())];
                if let Some(payload) = payload {
                    fields.push(("val".into(), *payload));
                }
                self.mir_record(ctx, fields)?.into()
            }
            _ => perry_mir::VariantType::get(ctx, cases).into(),
        };
        self.callables.insert(ty.index());
        Ok(Some(absent.map_or(present, |state| {
            perry_mir::OptionalType::get_with(ctx, present, state).into()
        })))
    }
}
