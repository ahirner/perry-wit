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
    pub project_root: Option<PathBuf>,
}

impl Default for SdkOptions {
    fn default() -> Self {
        Self {
            wit_dir: PathBuf::from("wit"),
            world: None,
            out_dir: PathBuf::from(".perry/types"),
            project_root: None,
        }
    }
}

/// Result of SDK generation containing created file paths.
#[derive(Debug, Clone)]
pub struct SdkResult {
    pub types_path: PathBuf,
    pub tsconfig_path: Option<PathBuf>,
}

/// Generates `.perry/types/world.d.ts` and default `tsconfig.json` if not present.
pub fn generate_sdk_files(options: &SdkOptions) -> Result<SdkResult> {
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

    // Determine project root for tsconfig.json
    let project_root = options.project_root.clone().unwrap_or_else(|| {
        if options.out_dir.ends_with(".perry/types") {
            options
                .out_dir
                .parent()
                .and_then(|p| p.parent())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        } else {
            PathBuf::from(".")
        }
    });

    let tsconfig_path = project_root.join("tsconfig.json");
    let generated_tsconfig = if !tsconfig_path.exists() {
        let tsconfig_content = tsconfig::generate_default_tsconfig();
        fs::write(&tsconfig_path, tsconfig_content).with_context(|| {
            format!(
                "Failed to write tsconfig.json at {}",
                tsconfig_path.display()
            )
        })?;
        Some(tsconfig_path)
    } else {
        Some(tsconfig_path)
    };

    Ok(SdkResult {
        types_path: dts_path,
        tsconfig_path: generated_tsconfig,
    })
}
