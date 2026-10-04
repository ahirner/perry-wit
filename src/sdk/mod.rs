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
    pub entry: PathBuf,
}

impl Default for SdkOptions {
    fn default() -> Self {
        Self {
            wit_dir: PathBuf::from("wit"),
            world: None,
            out_dir: PathBuf::from(".perry/types"),
            project_root: None,
            entry: PathBuf::from("src/index.ts"),
        }
    }
}

/// Result of SDK generation containing created file paths.
#[derive(Debug, Clone)]
pub struct SdkResult {
    pub types_path: PathBuf,
    pub imports_path: PathBuf,
    pub check_path: PathBuf,
    pub tsconfig_path: Option<PathBuf>,
}

/// Generates world/import declarations, an implementation check, and a missing tsconfig.
pub fn generate_sdk_files(options: &SdkOptions) -> Result<SdkResult> {
    fs::create_dir_all(&options.out_dir).with_context(|| {
        format!(
            "Failed to create SDK output directory at {}",
            options.out_dir.display()
        )
    })?;

    let (resolve, package) = crate::component::wit::resolve_wit(&options.wit_dir)?;
    let world = resolve.select_world(&[package], options.world.as_deref())?;
    let dts = codegen::generate_world_declarations(&resolve, &resolve.worlds[world])?;
    let imports_path = options.out_dir.join("imports.d.ts");
    fs::write(
        &imports_path,
        codegen::generate_import_declarations(&resolve, &resolve.worlds[world]),
    )?;
    fs::write(
        options.out_dir.join("p3.d.ts"),
        include_str!("../../types/p3.d.ts"),
    )?;
    let dts = format!(
        "/// <reference path=\"./imports.d.ts\" />\n/// <reference path=\"./p3.d.ts\" />\n{dts}"
    );

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
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf()
        } else {
            PathBuf::from(".")
        }
    });

    let tsconfig_path = project_root.join("tsconfig.json");
    let entry = project_root.join(&options.entry);
    let entry_import = relative_path(&options.out_dir, &entry.with_extension(""))?;
    let check_path = options.out_dir.join("implementation-check.ts");
    let check = format!(
        "import * as implementation from {};\nimport type {{ ComponentImplementation }} from './world';\nconst checked: ComponentImplementation = implementation;\nexport {{ checked }};\n",
        serde_json::to_string(&entry_import)?
    );
    fs::write(&check_path, check)?;
    let generated_tsconfig = if !tsconfig_path.exists() {
        let mut config: serde_json::Value =
            serde_json::from_str(&tsconfig::generate_default_tsconfig())?;
        config["files"] = serde_json::json!([relative_path(&project_root, &check_path)?]);
        let tsconfig_content = serde_json::to_string_pretty(&config)?;
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
        imports_path,
        check_path,
        tsconfig_path: generated_tsconfig,
    })
}

fn relative_path(from: &Path, to: &Path) -> Result<String> {
    fn normalized(path: &Path) -> Result<PathBuf> {
        let mut result = PathBuf::new();
        for component in std::path::absolute(path)?.components() {
            if component == std::path::Component::ParentDir {
                result.pop();
            } else {
                result.push(component);
            }
        }
        Ok(result)
    }
    let from = normalized(from)?;
    let to = normalized(to)?;
    let common = from
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut relative = PathBuf::from(".");
    for _ in from.components().skip(common) {
        relative.push("..");
    }
    relative.extend(to.components().skip(common));
    Ok(relative.to_string_lossy().replace('\\', "/"))
}
