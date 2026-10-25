//! TypeScript declarations for a resolved component contract.
mod codegen;

use crate::Artifact;
use anyhow::Result;
use wit_parser::{Resolve, WorldId};

pub const RUNTIME_DECLARATIONS: &str = include_str!("runtime.d.ts");

pub fn declarations(resolve: &Resolve, world: WorldId) -> Result<Vec<Artifact>> {
    let world = &resolve.worlds[world];
    let declarations = codegen::generate_world_declarations(resolve, world)?;
    Ok(vec![
        Artifact {
            name: "world.d.ts".into(),
            contents: format!("/// <reference path=\"./runtime.d.ts\" />\n/// <reference path=\"./imports.d.ts\" />\n{declarations}").into_bytes(),
        },
        Artifact {
            name: "imports.d.ts".into(),
            contents: codegen::generate_import_declarations(resolve, world).into_bytes(),
        },
        Artifact {
            name: "runtime.d.ts".into(),
            contents: RUNTIME_DECLARATIONS.as_bytes().to_vec(),
        },
    ])
}

/// Generate the runtime surface and, when supplied, declarations for a contract.
pub fn generate(wit: Option<&crate::Wit>, world: Option<&str>) -> Result<Vec<Artifact>> {
    if let Some(wit) = wit {
        let (resolve, package) = crate::resolution::resolve_contents(wit)?;
        let world = crate::resolution::select_world(&resolve, package, world)?;
        declarations(&resolve, world)
    } else {
        Ok(vec![Artifact {
            name: "runtime.d.ts".into(),
            contents: RUNTIME_DECLARATIONS.as_bytes().to_vec(),
        }])
    }
}
