//! TypeScript sources, parsing, module resolution and declaration locations.
use anyhow::{Context, Result, ensure};
pub use locations::Locations;
pub use modules::{Loaded, load, resolve};
pub use parser::parse;
pub use perry_parser::Spanned;
pub use perry_parser::swc_ecma_ast as ast;
use std::collections::BTreeMap;
pub use swc_common::Span;
pub use swc_ecma_visit::{Visit, VisitWith};
pub use validation::validate;
pub mod files;
mod locations;
mod modules;
mod parser;
mod validation;

#[derive(Clone, Debug, Default)]
pub struct Source {
    /// Optional diagnostic name. For a TypeScript entrypoint, this also serves as its logical identity.
    pub name: Option<String>,
    pub contents: Vec<u8>,
}

impl From<&[u8]> for Source {
    fn from(contents: &[u8]) -> Self {
        Self {
            contents: contents.to_vec(),
            name: None,
        }
    }
}

/// A supplied dependency module resolved from in-memory contents by package-relative name.
#[derive(Clone, Debug)]
pub struct Module {
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub source: Source,
    pub modules: Vec<Module>,
}

pub fn text<'a>(contents: &'a [u8], name: Option<&str>) -> Result<&'a str> {
    std::str::from_utf8(contents)
        .with_context(|| format!("{} is not UTF-8", name.unwrap_or("input")))
}

fn module_name(name: &str) -> Result<&str> {
    ensure!(
        !name.is_empty()
            && !name.contains(['\\', '\0', ':'])
            && name
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "module identity must be package-relative: {name:?}"
    );
    Ok(name)
}

pub fn program(input: &Program) -> Result<(&str, &str, BTreeMap<String, String>)> {
    let identity = module_name(input.source.name.as_deref().unwrap_or("input.ts"))?;
    let mut modules = BTreeMap::new();
    for source in &input.modules {
        let name = module_name(&source.name)?;
        ensure!(
            name != identity && !modules.contains_key(name),
            "duplicate module identity: {name}"
        );
        modules.insert(
            name.to_owned(),
            text(&source.contents, Some(name))?.to_owned(),
        );
    }
    Ok((
        text(&input.source.contents, Some(identity))?,
        identity,
        modules,
    ))
}
