//! Content-based compiler API over caller-supplied sources and modules.
//!
//! [`compile`] validates a WIT world and returns its core module plus an optional
//! [`crate::ComponentOutput`] containing both component forms. [`Options::core_only`]
//! returns only the world-bound core module. [`compile_core`] exports raw TypeScript
//! functions with core signatures.
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

use anyhow::Result;

pub use perry_contract::{Artifact, Package, Wit};
use perry_source::program;
pub use perry_source::{Module, Program, Source};

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub world: Option<String>,
    /// Emit the linked core Wasm module directly while preserving the selected WIT world's ABI.
    pub core_only: bool,
}

/// Compile supplied TypeScript and WIT contents with the embedded standard packages.
fn compile_impl(input: &Program, wit: &Wit, options: &Options) -> Result<crate::Compiled> {
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
}

/// Compile raw TypeScript exports with core signatures from supplied in-memory sources.
fn compile_core_impl(input: &Program) -> Result<Vec<u8>> {
    let (source, identity, modules) = program(input)?;
    crate::compiler::compile_core_sources(source, identity, &modules, false)
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
) -> std::result::Result<crate::Compiled, crate::CompileError> {
    compile_impl(input, wit, options).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::InvalidInput)
    })
}

pub fn compile_core(input: &Program) -> std::result::Result<Vec<u8>, crate::CompileError> {
    compile_core_impl(input).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::InvalidInput)
    })
}

pub fn generate_sdk(
    wit: Option<&Wit>,
    world: Option<&str>,
) -> std::result::Result<Vec<Artifact>, crate::CompileError> {
    perry_contract::sdk::generate(wit, world).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::WitResolution)
    })
}

/// Derive named WIT contents without accessing the filesystem or compiling Wasm.
///
/// ```
/// # fn main() -> Result<(), perry_wit::CompileError> {
/// use perry_wit::api::{self, Options, Program, Wit};
/// let program = Program {
///     source: b"export function twice(n: number): number { return n * 2; }".as_slice().into(),
///     ..Default::default()
/// };
/// let files = api::generate_wit(&program, None)?;
/// let contract = Wit::from(files);
/// let compiled = api::compile(&program, &contract, &Options::default())?;
/// # assert!(compiled.component.is_some());
/// # Ok(())
/// # }
/// ```
pub fn generate_wit(
    input: &Program,
    world: Option<&str>,
) -> std::result::Result<Vec<Artifact>, crate::CompileError> {
    perry_contract::generate_wit(input, world).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::UnsupportedType)
    })
}
