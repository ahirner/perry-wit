//! Content-based compiler API over caller-supplied sources and modules.
//!
//! [`compile`] validates a WIT world and returns its core module plus an optional
//! [`crate::ComponentOutput`] containing both component forms. [`Options::core_only`]
//! returns only the world-bound core module. [`compile_core`] exports raw TypeScript
//! functions with core signatures.
//!
//! ```
//! # fn main() -> Result<(), perry_wit::CompileError> {
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

use perry_wit_diagnostics::{Result, ensure_diag};
use std::collections::BTreeMap;

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
    /// Emit the linked core Wasm module directly while preserving the selected WIT world's ABI.
    pub core_only: bool,
}

pub use perry_frontend::sdk::Declaration as Artifact;

pub(crate) fn text<'a>(contents: &'a [u8], name: Option<&str>) -> Result<&'a str> {
    std::str::from_utf8(contents).map_err(|error| {
        crate::Diagnostic::new(
            crate::codes::INVALID_UTF8,
            format!("{} is not UTF-8", name.unwrap_or("input")),
        )
        .with_source_notes(&error)
    })
}

fn module_name(name: &str) -> Result<&str> {
    ensure_diag!(
        !name.is_empty()
            && !name.contains(['\\', '\0', ':'])
            && name
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        crate::codes::INVALID_INPUT,
        "module identity must be package-relative: {name:?}"
    );
    Ok(name)
}

fn program(input: &Program) -> Result<(&str, &str, BTreeMap<String, String>)> {
    let identity = module_name(input.source.name.as_deref().unwrap_or("input.ts"))?;
    let mut modules = BTreeMap::new();
    for source in &input.modules {
        let name = module_name(&source.name)?;
        ensure_diag!(
            name != identity && !modules.contains_key(name),
            crate::codes::INVALID_INPUT,
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

/// Compile in-memory TypeScript source modules and WIT package definitions into a
/// validated WASI 0.3 WebAssembly component.
///
/// ```
/// # fn main() -> Result<(), perry_wit::CompileError> {
/// use perry_wit::api::{self, Options, Program, Wit};
///
/// let program = Program {
///     source: br#"
///         export function authorize(token: string): boolean {
///             return token.startsWith("Bearer ") && token.length > 16;
///         }
///     "#
///     .as_slice()
///     .into(),
///     modules: vec![],
/// };
///
/// let wit = Wit {
///     application: br#"
///         package example:auth@0.1.0;
///         world filter {
///             export authorize: func(token: string) -> bool;
///         }
///     "#
///     .as_slice()
///     .into(),
///     dependencies: vec![],
/// };
///
/// let compiled = api::compile(&program, &wit, &Options::default())?;
/// assert!(!compiled.core.is_empty());
/// assert!(compiled.component.is_some_and(|component| !component.bytes.is_empty()));
/// # Ok(())
/// # }
/// ```
pub fn compile(
    input: &Program,
    wit: &Wit,
    options: &Options,
) -> Result<crate::Compiled, crate::CompileError> {
    let (source, identity, modules) = program(input)?;
    let (resolve, package) = crate::component::wit::resolve_contents(wit)?;
    let world = crate::component::wit::select_world(&resolve, package, options.world.as_deref())?;
    crate::compiler::compile_sources(
        source,
        identity,
        &modules,
        &resolve,
        world,
        !options.core_only,
        false,
    )
    .map_err(Into::into)
}

/// Compile raw TypeScript exports with core signatures from supplied in-memory sources.
pub fn compile_core(input: &Program) -> Result<Vec<u8>, crate::CompileError> {
    let (source, identity, modules) = program(input)?;
    crate::compiler::compile_core_sources(source, identity, &modules, false).map_err(Into::into)
}

/// Generate declaration contents for the selected world.
pub fn generate_sdk(wit: &Wit, world: Option<&str>) -> Result<Vec<Artifact>, crate::CompileError> {
    let (resolve, package) = crate::component::wit::resolve_contents(wit)?;
    let world = crate::component::wit::select_world(&resolve, package, world)?;
    perry_frontend::sdk::declarations(&resolve, world).map_err(Into::into)
}
