//! Strict incremental UTF-8 decoding with traced pending bytes and immutable text results.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::types::Type as HirType;
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, runtime};

pub(crate) const DECODER_TYPE: &str = "__perry_internal_text_decoder";

#[derive(Clone, Copy)]
pub(crate) struct DecoderHelpers {
    pub(crate) new: Func,
    pub(crate) decode: Func,
}

pub(crate) fn is_decoder(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == DECODER_TYPE || name == "TextDecoder")
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<DecoderHelpers> {
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("decoder/runtime.wat"),
        &BTreeMap::from([("realloc", allocator.realloc)]),
    )?;
    Ok(DecoderHelpers {
        new: functions["decoder.new"],
        decode: functions["decoder.decode"],
    })
}
