//! Component bindings for the same byte-oriented API used by native embedders.

wit_bindgen::generate!({ path: "wit/compiler", world: "compiler" });

use exports::perry::compiler::api::{
    Artifact, CompileOptions, Compiled, ComponentOutput, Diagnostic, DiagnosticCode,
    DiagnosticSeverity, Guest, Module, Program, Source, SourceSpan, WitInput, WitPackage,
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

impl From<perry_wit::PerryDiagnostic> for Diagnostic {
    fn from(diagnostic: perry_wit::PerryDiagnostic) -> Self {
        Self {
            code: match diagnostic.code {
                perry_wit::DiagnosticCode::ParseError => DiagnosticCode::ParseError,
                perry_wit::DiagnosticCode::UnsupportedOperator => {
                    DiagnosticCode::UnsupportedOperator
                }
                perry_wit::DiagnosticCode::UnsupportedSpread => DiagnosticCode::UnsupportedSpread,
                perry_wit::DiagnosticCode::MissingTypeAnnotation => {
                    DiagnosticCode::MissingTypeAnnotation
                }
                perry_wit::DiagnosticCode::UnsupportedType => DiagnosticCode::UnsupportedType,
                perry_wit::DiagnosticCode::UnsupportedExpression => {
                    DiagnosticCode::UnsupportedExpression
                }
                perry_wit::DiagnosticCode::IncompatibleRecordLayout => {
                    DiagnosticCode::IncompatibleRecordLayout
                }
                perry_wit::DiagnosticCode::MissingRequiredField => {
                    DiagnosticCode::MissingRequiredField
                }
                perry_wit::DiagnosticCode::AbsenceMismatch => DiagnosticCode::AbsenceMismatch,
                perry_wit::DiagnosticCode::InterfaceMismatch => DiagnosticCode::InterfaceMismatch,
                perry_wit::DiagnosticCode::ExportMismatch => DiagnosticCode::ExportMismatch,
                perry_wit::DiagnosticCode::ImportSignatureMismatch => {
                    DiagnosticCode::ImportSignatureMismatch
                }
                perry_wit::DiagnosticCode::ModuleResolution => DiagnosticCode::ModuleResolution,
                perry_wit::DiagnosticCode::InvalidInput => DiagnosticCode::InvalidInput,
                perry_wit::DiagnosticCode::WitResolution => DiagnosticCode::WitResolution,
                perry_wit::DiagnosticCode::CodegenFailure => DiagnosticCode::CodegenFailure,
                perry_wit::DiagnosticCode::MissingErrorChannel => {
                    DiagnosticCode::MissingErrorChannel
                }
                perry_wit::DiagnosticCode::Io => DiagnosticCode::Io,
                perry_wit::DiagnosticCode::InternalCompilerError => {
                    DiagnosticCode::InternalCompilerError
                }
            },
            severity: match diagnostic.severity {
                perry_wit::DiagnosticSeverity::Error => DiagnosticSeverity::Error,
                perry_wit::DiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
                perry_wit::DiagnosticSeverity::Info => DiagnosticSeverity::Info,
            },
            message: diagnostic.message,
            span: diagnostic.span.map(|span| SourceSpan {
                file: span.file.to_string_lossy().into_owned(),
                start_line: span.start_line,
                start_column: span.start_column,
                end_line: span.end_line,
                end_column: span.end_column,
                start_byte: span.start_byte as u64,
                end_byte: span.end_byte as u64,
            }),
            notes: diagnostic.notes,
            help: diagnostic.help,
        }
    }
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
        .map_err(|error| error.diagnostics.into_iter().map(Into::into).collect())
    }
    fn compile_core(program: Program) -> Result<Vec<u8>, Vec<Diagnostic>> {
        api::compile_core(&program.into())
            .map_err(|error| error.diagnostics.into_iter().map(Into::into).collect())
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
            .map_err(|error| error.diagnostics.into_iter().map(Into::into).collect())
    }
}

export!(Compiler);
