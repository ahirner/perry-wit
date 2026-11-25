//! Local storage and statement contexts for each solved instance.
use super::*;

impl<'h> InstancePlanner<'h, '_> {
    pub(super) fn local_storage(
        &mut self,
        instance: InstanceId,
        id: LocalId,
        storage: crate::engine::Storage,
    ) -> Result<()> {
        let owner = self.solved.body_owner(instance);
        let id = self
            .index
            .local_idx(id)
            .ok_or_else(|| Diagnostic::internal("storage for an unknown lexical binding"))?;
        self.decisions
            .entry(owner)
            .or_default()
            .locals
            .insert(id, storage);
        Ok(())
    }

    fn resolve_let_ty(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        name: &str,
        ty: &HirType,
    ) -> Result<Option<Ty>> {
        let resolved = self.local_ty(inst_id, id);
        let local_flags = self
            .index
            .local_idx(id)
            .map(|li| self.index.locals[li].flags);
        let is_inferred = !self.declarations.locals.contains_key(&id)
            || local_flags.is_none_or(|f| f.contains(LocalFlags::INFERRED));
        match resolved {
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(ty, HirType::Any | HirType::Unknown)
                        || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                        || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
            {
                self.prepare_layouts(t)?;
                Ok(Some(t))
            }
            Some(t)
                if matches!(
                    self.types.kind(t),
                    TyKind::Cell {
                        kind: CellKind::Dict,
                        ..
                    }
                ) =>
            {
                self.prepare_layouts(t)?;
                Ok(Some(t))
            }
            Some(t)
                if let TyKind::Cell { loc, kind } = self.types.kind(t)
                    && kind.is_array()
                    && !self.is_resolved(t) =>
            {
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "an empty array literal needs a declared element type"
                )
                .with_help(format!(
                    "Add a concrete element type annotation to '{name}'."
                ));
                if self.solved.self_nested_locs.contains(loc.index()) {
                    err = err.with_note("its inferred element type nests itself");
                }
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            Some(Ty::OPEN)
                if is_inferred
                    && self.index.local_idx(id).is_some_and(|lidx| {
                        self.solved.self_nested_locals.contains(lidx.index())
                    }) =>
            {
                let err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer type of '{name}': inferred type nests itself"
                )
                .with_help("Add a concrete type annotation to this binding.");
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn prepare_local(
        &mut self,
        instance: InstanceId,
        binding: &crate::engine::boundary::SourceLocal<'_>,
    ) -> Result<()> {
        let id = self.index.locals[binding.local].id;
        let storage = if let Some(t) = self.local_ty(instance, id)
            && let TyKind::Func(identity) = self.types.kind(t)
            && !self.is_env_template(identity.template)
        {
            crate::engine::Storage::ErasedFunction(None)
        } else {
            let solved = self.resolve_let_ty(instance, id, binding.name, binding.ty)?;
            if self.is_boxed_local(id) {
                crate::engine::Storage::Boxed {
                    ty: self.boxed_type(id, solved)?,
                    init: self.cell_init(id),
                }
            } else if let Some(ty) = solved {
                crate::engine::Storage::Value(ty)
            } else {
                return Ok(());
            }
        };
        self.decisions
            .entry(self.solved.body_owner(instance))
            .or_default()
            .locals
            .insert(binding.local, storage);
        Ok(())
    }

    pub(super) fn boxed_type(&self, id: LocalId, solved: Option<Ty>) -> Result<Ty> {
        solved
            .or_else(|| {
                self.index
                    .local_idx(id)
                    .map(|local| self.index.locals[local].declared)
            })
            .ok_or_else(|| Diagnostic::internal("boxed local has no declaration"))
    }
}
