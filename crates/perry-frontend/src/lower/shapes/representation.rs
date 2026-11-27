//! Source schemas to the existing MIR representation contracts.
use super::*;
use crate::engine::types::WitView;
use crate::engine::types::{FuncIdentity, Idx, func_tag};

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn foreign_value_type(
        &mut self,
        ctx: &Context,
        entry: &crate::wit::Foreign,
    ) -> Result<TypeHandle> {
        let (value, view) = match entry {
            crate::wit::Foreign::Type(value) => (*value, WitView::Boundary),
            crate::wit::Foreign::Input(value) => (*value, WitView::Input),
        };
        let ty = self.types.source.wit_type(ctx, value, view);
        Ok(self.resolved_binding(ctx, ty)?.carrier)
    }
}

impl<T> Shapes<'_, T> {
    pub(in crate::lower) fn function_record(
        &mut self,
        ctx: &Context,
        payload: Option<TypeHandle>,
    ) -> Result<TypedHandle<RecordType>> {
        let mut fields = vec![("tag".into(), StringType::get(ctx).into())];
        fields.extend(payload.map(|ty| ("val".into(), ty)));
        self.mir_record(ctx, fields)
    }

    pub(in crate::lower) fn callable_environment(
        &self,
        function: FuncIdentity,
    ) -> Option<TypeHandle> {
        self.specialization
            .instances
            .environments
            .get(&function)
            .map(|layout| layout.record.into())
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
            [(_, payload)] => self.function_record(ctx, *payload)?.into(),
            _ => perry_mir::VariantType::get(ctx, cases).into(),
        };
        self.callables.insert(ty.index());
        Ok(Some(absent.map_or(present, |state| {
            perry_mir::OptionalType::get_with(ctx, present, state).into()
        })))
    }
}
