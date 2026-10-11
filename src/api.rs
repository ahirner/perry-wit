//! Content-based compiler API. Module names identify supplied contents, never host files.
//!
//! [`compile`] checks a WIT world and returns its core plus an optional [`crate::ComponentOutput`]
//! containing both component forms. [`Options::core_only`] omits that record; it does not
//! bypass WIT. [`compile_core`] instead exports raw TypeScript functions without WIT.
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use perry_wit::api::{compile_core, Program};
//! let program = Program {
//!     source: b"export function twice(n: number): number { return n * 2; }".as_slice().into(),
//!     ..Default::default()
//! };
//! let core = compile_core(&program)?;
//! # assert!(!core.is_empty());
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Source {
    /// Optional diagnostic name; for a TypeScript entrypoint, also its logical identity.
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

/// A supplied dependency module. Imports resolve against this identity, never host files.
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

#[derive(Clone, Debug)]
pub enum Package {
    Sources(Vec<Source>),
    Encoded(Vec<u8>),
}

impl From<&[u8]> for Package {
    fn from(contents: &[u8]) -> Self {
        Self::Sources(vec![contents.into()])
    }
}

#[derive(Clone, Debug)]
pub struct Wit {
    pub application: Package,
    pub dependencies: Vec<Package>,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub world: Option<String>,
    /// Omit component wrapping; the core still implements the selected WIT world's ABI.
    pub core_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub contents: Vec<u8>,
}

pub(crate) fn text<'a>(contents: &'a [u8], name: Option<&str>) -> Result<&'a str> {
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

pub(crate) fn program(input: &Program) -> Result<(&str, &str, BTreeMap<String, String>)> {
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

/// Compile supplied TypeScript and WIT contents with the embedded standard packages.
pub fn compile(input: &Program, wit: &Wit, options: &Options) -> Result<crate::Compiled> {
    let (source, identity, modules) = program(input)?;
    let (resolve, package) = crate::component::wit::resolve_contents(wit)?;
    let world = resolve.select_world(&[package], options.world.as_deref())?;
    crate::compiler::compile_sources(
        source,
        identity,
        &modules,
        &resolve,
        world,
        !options.core_only,
        false,
    )
}

/// Compile raw TypeScript exports without WIT or filesystem access.
/// Unlike [`compile`] with [`Options::core_only`], these exports use raw core signatures,
/// not the canonical ABI of a WIT world.
pub fn compile_core(input: &Program) -> Result<Vec<u8>> {
    let (source, identity, modules) = program(input)?;
    crate::compiler::compile_core_sources(source, identity, &modules, false)
}

/// Generate declaration contents. Project layout and writing belong to the caller.
pub fn generate_sdk(wit: &Wit, world: Option<&str>) -> Result<Vec<Artifact>> {
    let (resolve, package) = crate::component::wit::resolve_contents(wit)?;
    let world = resolve.select_world(&[package], world)?;
    crate::sdk::declarations(&resolve, world)
}
