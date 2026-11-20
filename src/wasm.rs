//! Component bindings for the same byte-oriented API used by native embedders.

wit_bindgen::generate!({ path: "wit/compiler", world: "compiler" });

use exports::perry::compiler::api::{
    Artifact, CompileOptions, Compiled, ComponentOutput, Diagnostic, ErrorKind, Guest, Module,
    Note, Program, Source, SourceSpan, WitInput, WitPackage,
};
use perry_wit::api;

impl From<Source> for api::Source {
    fn from(source: Source) -> Self {
        Self {
            name: source.name,
            contents: source.contents,
        }
    }
}
impl From<Module> for api::Module {
    fn from(module: Module) -> Self {
        Self {
            name: module.name,
            contents: module.contents,
        }
    }
}
impl From<Program> for api::Program {
    fn from(program: Program) -> Self {
        Self {
            source: program.source.into(),
            modules: program.modules.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<WitPackage> for api::Package {
    fn from(package: WitPackage) -> Self {
        match package {
            WitPackage::Sources(sources) => {
                Self::Sources(sources.into_iter().map(Into::into).collect())
            }
            WitPackage::Encoded(bytes) => Self::Encoded(bytes),
        }
    }
}
impl From<WitInput> for api::Wit {
    fn from(wit: WitInput) -> Self {
        Self {
            application: wit.application.into(),
            dependencies: wit.dependencies.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<perry_wit::ErrorKind> for ErrorKind {
    fn from(kind: perry_wit::ErrorKind) -> Self {
        match kind {
            perry_wit::ErrorKind::InvalidInput => Self::InvalidInput,
            perry_wit::ErrorKind::Io => Self::Io,
            perry_wit::ErrorKind::Wit => Self::Wit,
            perry_wit::ErrorKind::Syntax => Self::Syntax,
            perry_wit::ErrorKind::Module => Self::Module,
            perry_wit::ErrorKind::TypeCheck => Self::TypeCheck,
            perry_wit::ErrorKind::Unsupported => Self::Unsupported,
            perry_wit::ErrorKind::Contract => Self::Contract,
            perry_wit::ErrorKind::Internal => Self::Internal,
        }
    }
}

fn to_source_span(span: &perry_wit::SourceSpan) -> SourceSpan {
    SourceSpan {
        file: span.file.to_string_lossy().into_owned(),
        start_line: span.start_line,
        start_column: span.start_column,
        end_line: span.end_line,
        end_column: span.end_column,
        start_byte: span.start_byte as u64,
        end_byte: span.end_byte as u64,
    }
}

impl From<perry_wit::Diagnostic> for Diagnostic {
    fn from(diagnostic: perry_wit::Diagnostic) -> Self {
        Self {
            kind: diagnostic.kind().into(),
            code: diagnostic.code().name().to_owned(),
            message: diagnostic.message().to_owned(),
            span: diagnostic.span().map(to_source_span),
            notes: diagnostic
                .notes()
                .iter()
                .map(|note| Note {
                    message: note.message().to_owned(),
                    span: note.span().map(to_source_span),
                })
                .collect(),
            help: diagnostic.help().map(str::to_owned),
        }
    }
}

fn into_wit_errors(error: perry_wit::CompileError) -> Vec<Diagnostic> {
    error
        .into_diagnostics()
        .into_iter()
        .map(Into::into)
        .collect()
}

struct Compiler;
impl Guest for Compiler {
    fn compile(
        program: Program,
        wit: WitInput,
        options: CompileOptions,
    ) -> Result<Compiled, Vec<Diagnostic>> {
        api::compile(
            &program.into(),
            &wit.into(),
            &api::Options {
                world: options.world,
                core_only: options.core_only,
            },
        )
        .map(|compiled| Compiled {
            core: compiled.core,
            component: compiled.component.map(|component| ComponentOutput {
                bytes: component.bytes,
                stripped: component.stripped,
            }),
            allocation_report: compiled.allocation_report,
        })
        .map_err(into_wit_errors)
    }
    fn compile_core(program: Program) -> Result<Vec<u8>, Vec<Diagnostic>> {
        api::compile_core(&program.into()).map_err(into_wit_errors)
    }
    fn generate_sdk(
        wit: WitInput,
        world: Option<String>,
    ) -> Result<Vec<Artifact>, Vec<Diagnostic>> {
        api::generate_sdk(&wit.into(), world.as_deref())
            .map(|artifacts| {
                artifacts
                    .into_iter()
                    .map(|artifact| Artifact {
                        name: artifact.name,
                        contents: artifact.contents,
                    })
                    .collect()
            })
            .map_err(into_wit_errors)
    }
}

export!(Compiler);
