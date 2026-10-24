//! TypeScript declarations for a resolved component contract.
mod codegen;

use anyhow::Result;
use wit_parser::{Resolve, WorldId};

pub const RUNTIME_DECLARATIONS: &str = include_str!("runtime.d.ts");

pub struct Declaration {
    pub name: String,
    pub contents: Vec<u8>,
}

pub fn declarations(resolve: &Resolve, world: WorldId) -> Result<Vec<Declaration>> {
    let world = &resolve.worlds[world];
    let declarations = codegen::generate_world_declarations(resolve, world)?;
    Ok(vec![
        Declaration {
            name: "world.d.ts".into(),
            contents: format!("/// <reference path=\"./runtime.d.ts\" />\n/// <reference path=\"./imports.d.ts\" />\n{declarations}").into_bytes(),
        },
        Declaration {
            name: "imports.d.ts".into(),
            contents: codegen::generate_import_declarations(resolve, world).into_bytes(),
        },
        Declaration {
            name: "runtime.d.ts".into(),
            contents: RUNTIME_DECLARATIONS.as_bytes().to_vec(),
        },
    ])
}
