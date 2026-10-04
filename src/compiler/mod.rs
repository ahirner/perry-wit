//! Production TypeScript → HIR → WAFFLE → WASI 0.3 compilation.

use crate::{
    component::wit::resolve_wit,
    strip,
    waffle_backend::{WaffleCompileOptions, compile_typescript_for_world},
};
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub wit_dir: PathBuf,
    pub world: Option<String>,
    pub core_only: bool,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            wit_dir: "wit".into(),
            world: None,
            core_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub core: Vec<u8>,
    pub component: Option<Vec<u8>>,
    pub stripped: Option<Vec<u8>>,
}

pub fn compile_file(input_file: &Path, options: &CompileOptions) -> Result<Compiled> {
    let source = fs::read_to_string(input_file)
        .with_context(|| format!("Reading TypeScript source {}", input_file.display()))?;
    compile_typescript(&source, &input_file.to_string_lossy(), options)
}

pub fn compile_typescript(
    source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled> {
    let (resolve, package) = resolve_wit(&options.wit_dir)?;
    let world = resolve.select_world(&[package], options.world.as_deref())?;
    let compiled = compile_typescript_for_world(
        source,
        file_name,
        &WaffleCompileOptions {
            componentize: !options.core_only,
            ..Default::default()
        },
        resolve,
        world,
    )?;
    let stripped = compiled
        .component
        .as_deref()
        .map(strip::component)
        .transpose()?;
    Ok(Compiled {
        core: compiled.core,
        component: compiled.component,
        stripped,
    })
}
