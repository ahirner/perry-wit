//! Canonical parameter records when the flat argument limit is exceeded.
use super::*;
use anyhow::Context as _;

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
        params: &[AbiType],
        mir: &[TypeHandle],
        flat: &mut Vec<Value>,
    ) -> Result<Vec<Value>> {
        if params.iter().map(|ty| ty.flat().len()).sum::<usize>() <= MAX_FLAT_PARAMS {
            return params
                .iter()
                .zip(mir)
                .map(|(ty, mir)| self.lift_flat(ctx, ty, *mir, flat))
                .collect();
        }
        let area = flat.pop().context("missing indirect parameter record")?;
        let mut args = Vec::with_capacity(params.len());
        for ((ty, mir), offset) in params.iter().zip(mir).zip(record_offsets(params.iter())) {
            args.push(self.lift_load(ctx, ty, *mir, area, offset)?);
        }
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::CabiRelease, vec![area]);
        }
        Ok(args)
    }
}
