//! Local storage and statement contexts for each solved instance.
use super::*;

impl<'h> InstancePlanner<'h, '_, '_> {
    pub(super) fn prepare_local(
        &mut self,
        inst_id: InstanceId,
        binding: &crate::engine::boundary::SourceLocal<'_>,
    ) -> Result<()> {
        use crate::engine::Storage;
        let local = self.index.locals[binding.local];
        let id = local.id;
        let name = binding.name;
        let ty = binding.ty;
        let resolved = self.local_ty(inst_id, binding.local);
        let is_inferred = !self.declarations.locals.contains_key(&id)
            || local.flags.contains(LocalFlags::INFERRED);
        let solved = match resolved {
            Some(t)
                if let TyKind::Func(identity) = self.types.kind(t)
                    && !self.is_env_template(identity.template) =>
            {
                self.locals[inst_id].insert(binding.local, Storage::ErasedFunction(None));
                return Ok(());
            }
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(ty, HirType::Any | HirType::Unknown)
                        || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                        || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
            {
                Some(t)
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
                Some(t)
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
                return Err(self.attach_inst_chain(err, inst_id));
            }
            Some(Ty::OPEN)
                if is_inferred
                    && self
                        .solved
                        .self_nested_locals
                        .contains(binding.local.index()) =>
            {
                let err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer type of '{name}': inferred type nests itself"
                )
                .with_help("Add a concrete type annotation to this binding.");
                let err = self.annotate_local(err, id);
                return Err(self.attach_inst_chain(err, inst_id));
            }
            _ => None,
        };
        let storage = match self.cell_init(binding.local) {
            Some(init) => Storage::Boxed {
                ty: solved.unwrap_or(local.declared),
                init,
            },
            None => match solved {
                Some(ty) => Storage::Value(ty),
                None => return Ok(()),
            },
        };
        self.locals[inst_id].insert(binding.local, storage);
        Ok(())
    }
}
