//! Canonical parameter records when the flat argument limit is exceeded.
use super::*;
use perry_wit_diagnostics::Diagnostic;

pub(super) fn export_signature(params: &[AbiType]) -> Vec<Flat> {
    let flat: Vec<_> = params.iter().flat_map(AbiType::flat).collect();
    if flat.len() > MAX_FLAT_PARAMS {
        vec![Flat::I32]
    } else {
        flat
    }
}

impl FunctionLowering<'_, '_> {
    pub(super) fn lift_parameters(
        &mut self,
        ctx: &mut Context,
        export: &Export,
        mir: &[TypeHandle],
        flat: &mut Vec<Value>,
    ) -> Result<Vec<Value>> {
        let params = &export.params;
        if params.iter().map(|ty| ty.flat().len()).sum::<usize>() <= MAX_FLAT_PARAMS {
            return params
                .iter()
                .zip(mir)
                .enumerate()
                .map(|(i, (ty, mir))| {
                    self.lift_abi(ctx, ty, *mir, Canonical::Flat(flat))
                        .map_err(|e| e.with_fallback_span(export.annotation(i)))
                })
                .collect();
        }
        let area = flat
            .pop()
            .ok_or_else(|| Diagnostic::internal("missing indirect parameter record"))?;
        let mut args = Vec::with_capacity(params.len());
        for ((ty, mir), offset) in params.iter().zip(mir).zip(record_offsets(params.iter())) {
            args.push(
                self.lift_abi(ctx, ty, *mir, Canonical::Memory(area, offset))
                    .map_err(|e| e.with_fallback_span(export.annotation(args.len())))?,
            );
        }
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::CabiRelease, vec![area]);
        }
        Ok(args)
    }
}
