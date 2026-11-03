//! In-process component custom section stripping.

use anyhow::Result;
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
        component: &mut Component,
        section: CustomSectionReader<'_>,
    ) -> Result<(), Error<Self::Error>> {
        if section.name() == wit_parser::PackageMetadata::SECTION_NAME
            || section
                .name()
                .starts_with(crate::component::metadata::PREFIX)
        {
            component.section(&wasm_encoder::CustomSection {
                name: section.name().into(),
                data: section.data().into(),
            });
        }
        Ok(())
    }
}

/// Strips descriptive custom sections (names, producers) from a component.
pub(crate) fn component(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut component = Component::new();
    let mut parser = Parser::new(0);
    parser.set_features(WasmFeatures::all());
    Strip.parse_component(&mut component, parser, bytes)?;
    Ok(component.finish())
}
