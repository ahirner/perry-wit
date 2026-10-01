//! Zero-Config SDK generation and TypeScript developer experience.

pub mod codegen;
pub mod tsconfig;

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Configuration options for SDK type generation.
#[derive(Debug, Clone)]
pub struct SdkOptions {
    pub wit_dir: PathBuf,
    pub world: Option<String>,
    pub out_dir: PathBuf,
}

impl Default for SdkOptions {
    fn default() -> Self {
        Self {
            wit_dir: PathBuf::from("wit"),
            world: None,
            out_dir: PathBuf::from(".perry/types"),
        }
    }
}

/// Generates `.perry/types/world.d.ts` and default `tsconfig.json` if not present.
pub fn generate_sdk_files(options: &SdkOptions) -> Result<PathBuf> {
    fs::create_dir_all(&options.out_dir).with_context(|| {
        format!(
            "Failed to create SDK output directory at {}",
            options.out_dir.display()
        )
    })?;

    let (_world_name, dts) =
        codegen::generate_declarations_from_wit_dir(&options.wit_dir, options.world.as_deref())?;

    let dts_path = options.out_dir.join("world.d.ts");
    fs::write(&dts_path, dts)
        .with_context(|| format!("Failed to write declaration file at {}", dts_path.display()))?;

    // Also check if tsconfig.json exists in root
    let tsconfig_path = Path::new("tsconfig.json");
    if !tsconfig_path.exists() {
        let tsconfig_content = tsconfig::generate_default_tsconfig();
        let _ = fs::write(tsconfig_path, tsconfig_content);
    }

    Ok(dts_path)
}
