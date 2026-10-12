//! Component bindings for the same byte-oriented API used by native embedders.

wit_bindgen::generate!({ path: "wit/compiler", world: "compiler" });

use exports::perry::compiler::api::{
    Artifact, CompileOptions, Compiled, ComponentOutput, Guest, Module, Program, Source, WitInput,
    WitPackage,
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

struct Compiler;
impl Guest for Compiler {
    fn compile(
        program: Program,
        wit: WitInput,
        options: CompileOptions,
    ) -> Result<Compiled, String> {
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
        .map_err(|error| format!("{error:#}"))
    }
    fn compile_core(program: Program) -> Result<Vec<u8>, String> {
        api::compile_core(&program.into()).map_err(|error| format!("{error:#}"))
    }
    fn generate_sdk(wit: WitInput, world: Option<String>) -> Result<Vec<Artifact>, String> {
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
            .map_err(|error| format!("{error:#}"))
    }
}

export!(Compiler);
