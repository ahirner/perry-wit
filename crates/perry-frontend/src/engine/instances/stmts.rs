//! Local storage and statement contexts for each solved instance.
use super::*;

impl<'h> InstancePlanner<'h, '_, '_> {
    pub(super) fn prepare_local(
        &mut self,
        inst_id: InstanceId,
        binding: &crate::engine::boundary::SourceLocal<'_>,
    ) -> Result<()> {
        use crate::engine::Storage;
        let local = self.index.locals.rows[binding.local];
        let id = local.id;
        let name = binding.name;
        let loc = self.local_location(CaptureOwner::Instance(inst_id), binding.local);
        let resolved = loc.map(|loc| self.solved.locations[loc]);
        let is_inferred = !local.flags.is_explicit();
        let solved = match resolved {
            Some(t)
                if let TyKind::Func(identity) = self.types.kind(t)
                    && !self.is_env_template(identity.template) =>
            {
                self.locals[inst_id].push((binding.local, Storage::ErasedFunction(None)));
                return Ok(());
            }
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(local.declared, Ty::OPEN | Ty::DYNAMIC)
                        || self.types.has_type_parameter(local.declared)
                        || matches!(
                            self.types.kind(local.declared),
                            TyKind::Array(Ty::OPEN | Ty::DYNAMIC)
                        )
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
                    && loc
                        .is_some_and(|loc| self.solved.self_nested_locs.contains(loc.index())) =>
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
        self.locals[inst_id].push((binding.local, storage));
        Ok(())
    }
}
