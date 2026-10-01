//! Function signatures and type conversion for module linking.

use anyhow::Result;
use wasm_encoder::ValType;
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FuncSig {
    pub(crate) params: Vec<ValType>,
    pub(crate) results: Vec<ValType>,
}

pub(crate) fn to_func_sig(ft: &wasmparser::FuncType) -> Result<FuncSig> {
    let mut roundtrip = RoundtripReencoder;
    let params = ft
        .params()
        .iter()
        .map(|ty| roundtrip.val_type(*ty))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let results = ft
        .results()
        .iter()
        .map(|ty| roundtrip.val_type(*ty))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(FuncSig { params, results })
}
