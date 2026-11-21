//! In-process component custom section stripping.

use perry_wit_diagnostics::{Diagnostic, Result};
use std::convert::Infallible;
use wasm_encoder::{
    Component, Module,
    reencode::{Error, Reencode, ReencodeComponent},
};
use wasmparser::{CustomSectionReader, Parser, WasmFeatures};

struct Strip;

impl Reencode for Strip {
    type Error = Infallible;

    fn parse_custom_section(
        &mut self,
        _module: &mut Module,
        _section: CustomSectionReader<'_>,
    ) -> Result<(), Error<Self::Error>> {
        Ok(())
    }
}

impl ReencodeComponent for Strip {
    fn parse_component_custom_section(
        &mut self,
        _component: &mut Component,
        _section: CustomSectionReader<'_>,
    ) -> Result<(), Error<Self::Error>> {
        Ok(())
    }
}

/// Strips descriptive custom sections (names, producers) from a component.
pub(crate) fn component(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut component = Component::new();
    let mut parser = Parser::new(0);
    parser.set_features(WasmFeatures::all());
    Strip
        .parse_component(&mut component, parser, bytes)
        .map_err(|err| Diagnostic::internal_from("stripping component custom sections", &err))?;
    Ok(component.finish())
}
