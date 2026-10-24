//! Zero-Config SDK generation and TypeScript developer experience.

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
    pub initialize_tsconfig: bool,
}

impl Default for SdkOptions {
    fn default() -> Self {
        Self {
            wit_dir: PathBuf::from("wit"),
            world: None,
            out_dir: PathBuf::from(".perry/types"),
            project_root: None,
            entry: PathBuf::from("src/index.ts"),
            initialize_tsconfig: true,
        }
    }
}

/// Result of SDK generation containing created file paths.
#[derive(Debug, Clone)]
pub struct SdkResult {
    pub types_path: PathBuf,
    pub check_path: PathBuf,
    pub tsconfig_path: Option<PathBuf>,
}

/// Generates world/import declarations, an implementation check, and a missing tsconfig.
pub fn generate_sdk_files(
    options: &SdkOptions,
) -> std::result::Result<SdkResult, crate::CompileError> {
    generate_sdk_files_impl(options)
        .map_err(|error| crate::CompileError::from_error(error, crate::DiagnosticCode::Io))
}

fn generate_sdk_files_impl(options: &SdkOptions) -> Result<SdkResult> {
    let (resolve, package) = crate::component::wit::resolve_wit(&options.wit_dir)?;
    let world = crate::component::wit::select_world(&resolve, package, options.world.as_deref())?;
    let artifacts = declarations(&resolve, world).map_err(|error| {
        crate::PerryDiagnostic::from_error(error, crate::DiagnosticCode::WitResolution)
    })?;
    fs::create_dir_all(&options.out_dir).with_context(|| {
        format!(
            "Failed to create SDK output directory at {}",
            options.out_dir.display()
        )
    })?;

    for artifact in artifacts {
        let path = options.out_dir.join(artifact.name);
        fs::write(&path, artifact.contents)
            .with_context(|| format!("Failed to write declaration file at {}", path.display()))?;
    }
    let dts_path = options.out_dir.join("world.d.ts");

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
    let compiler_options = || {
        serde_json::json!({
            "target": "ES2022",
            "lib": ["ES2022", "DOM", "DOM.Iterable", "DOM.AsyncIterable"],
            "module": "ESNext",
            "moduleDetection": "force",
            "moduleResolution": "bundler",
            "allowImportingTsExtensions": true,
            "strict": true,
            "noEmit": true,
            "skipLibCheck": false,
            "types": []
        })
    };
    let generated_tsconfig = if !tsconfig_path.exists() && options.initialize_tsconfig {
        let config = serde_json::json!({
            "compilerOptions": compiler_options(),
            "include": ["src/**/*", "examples/**/*", "*.ts", ".perry/types/**/*"],
            "files": [relative_path(&project_root, &check_path)?]
        });
        fs::write(&tsconfig_path, serde_json::to_string_pretty(&config)?).with_context(|| {
            format!(
                "Failed to write tsconfig.json at {}",
                tsconfig_path.display()
            )
        })?;
        Some(tsconfig_path)
    } else if tsconfig_path.exists() {
        Some(tsconfig_path)
    } else {
        None
    };

    let mut check_config = serde_json::json!({
        "compilerOptions": compiler_options(),
        "files": ["implementation-check.ts"],
        "include": [],
        "exclude": []
    });
    if let Some(path) = &generated_tsconfig {
        check_config["extends"] = serde_json::json!(relative_path(&options.out_dir, path)?);
    }
    fs::write(
        options.out_dir.join("tsconfig.json"),
        serde_json::to_string_pretty(&check_config)?,
    )?;

    Ok(SdkResult {
        types_path: dts_path,
        check_path,
        tsconfig_path: generated_tsconfig,
    })
}

/// Generate portable declaration contents independently of a project layout.
pub(crate) fn declarations(
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<Vec<crate::api::Artifact>> {
    Ok(perry_frontend::sdk::declarations(resolve, world)?
        .into_iter()
        .map(|declaration| crate::api::Artifact {
            name: declaration.name,
            contents: declaration.contents,
        })
        .collect())
}

/// Emits the compiler's supported ambient runtime surface for editors and `tsc`.
pub fn write_runtime_declarations(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;
    fs::write(
        out_dir.join("runtime.d.ts"),
        perry_frontend::sdk::RUNTIME_DECLARATIONS,
    )?;
    Ok(())
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
