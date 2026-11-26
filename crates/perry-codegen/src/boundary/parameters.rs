//! Canonical parameter records when the flat argument limit is exceeded.
use super::*;
use perry_wit_diagnostics::Diagnostic;

impl FunctionLowering<'_, '_> {
    pub(super) fn lift_parameters(
        &mut self,
        ctx: &mut Context,
        export: &Export,
        mir: &[TypeHandle],
        flat: &mut Vec<Value>,
    ) -> Result<Vec<Value>> {
        let area = export
            .indirect_size
            .map(|_| {
                flat.pop()
                    .ok_or_else(|| Diagnostic::internal("missing indirect parameter record"))
            })
            .transpose()?;
        let mut source = match area {
            Some(area) => Canonical::Memory(area, 0),
            None => Canonical::Flat(flat),
        };
        let mut args = Vec::with_capacity(export.params.len());
        for (i, (ty, &mir)) in export.params.iter().zip(mir).enumerate() {
            args.push(
                self.lift_abi(ctx, ty, mir, source.at(ty.offset))
                    .map_err(|e| e.with_fallback_span(export.annotation(i)))?,
            );
        }
        if self.state.collectible
            && let Some(area) = area
        {
            self.runtime_void(ctx, Runtime::CabiRelease, vec![area]);
        }
        Ok(args)
    }
}
