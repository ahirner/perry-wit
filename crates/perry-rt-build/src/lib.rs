//! Metadata-driven builds of the runtime modules embedded in the compiler.

use perry_abi::layout;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = core::result::Result<T, Box<dyn core::error::Error>>;

/// Build or copy the declared runtime artifacts into `output`.
pub fn build(runtime: &Path, output: &Path) -> Result<()> {
    if output.exists() {
        fs::remove_dir_all(output)?;
    }
    fs::create_dir_all(output)?;
    for input in ["Cargo.toml", "Cargo.lock", ".cargo", "undefined.txt"] {
        println!("cargo:rerun-if-changed={}", runtime.join(input).display());
    }
    println!("cargo:rerun-if-env-changed=PERRY_RUNTIME_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=PERRY_PREBUILT_RUNTIME_DIR");
    println!("cargo:rerun-if-env-changed=RUSTC");
    if let Some(source) = env::var_os("PERRY_PREBUILT_RUNTIME_DIR") {
        let source = Path::new(&source);
        println!("cargo:rerun-if-changed={}", source.display());
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "wasm") {
                let bytes = fs::read(entry.path())?;
                wasmparser::Validator::new().validate_all(&bytes)?;
                fs::write(output.join(entry.file_name()), bytes)?;
            }
        }
        return Ok(());
    }
    compile_json_codecs(runtime, output)?;
    let metadata = cargo(runtime)
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--format-version=1",
            "--no-deps",
        ])
        .output()?;
    if !metadata.status.success() {
        return Err(format!(
            "read runtime metadata: {}",
            String::from_utf8_lossy(&metadata.stderr)
        )
        .into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or("runtime packages missing")?;
    for package in packages {
        let manifest = Path::new(
            package["manifest_path"]
                .as_str()
                .ok_or("runtime package manifest missing")?,
        );
        println!("cargo:rerun-if-changed={}", manifest.display());
        let directory = manifest
            .parent()
            .ok_or("runtime package directory missing")?;
        for input in ["src", "build.rs"] {
            let input = directory.join(input);
            if input.exists() {
                println!("cargo:rerun-if-changed={}", input.display());
            }
        }
        let Some(artifacts) = package["metadata"]["perry"]["artifacts"].as_array() else {
            continue;
        };
        let package = package["name"]
            .as_str()
            .ok_or("runtime package name missing")?;
        for artifact in artifacts {
            compile_artifact(runtime, output, package, artifact)?;
        }
    }
    Ok(())
}

fn cargo(runtime: &Path) -> Command {
    let mut command = Command::new(env::var_os("CARGO").expect("Cargo did not set CARGO"));
    command
        .current_dir(runtime)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CLIPPY_ARGS");
    command
}

fn compile_artifact(
    runtime: &Path,
    output: &Path,
    package: &str,
    artifact: &serde_json::Value,
) -> Result<()> {
    let name = artifact["name"].as_str().ok_or("artifact name missing")?;
    let profile = artifact["profile"]
        .as_str()
        .ok_or("artifact profile missing")?;
    let mut target = env::var_os("PERRY_RUNTIME_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| output.parent().unwrap_or(output).join("runtime-build"));
    if target.is_relative() {
        target = runtime.join(target);
    }
    let features = artifact["features"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| value.as_str().ok_or("invalid artifact feature"))
                .collect::<core::result::Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    if !features.is_empty() {
        target.push(features.join("-"));
    }
    let undefined = runtime.join("undefined.txt");
    let mut command = cargo(runtime);
    command
        .args([
            "rustc",
            "--locked",
            "--offline",
            "--lib",
            "--profile",
            profile,
            "--package",
            package,
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target);
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    let status = command
        .args([
            "--",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            &format!("-Clink-arg=--allow-undefined-file={}", undefined.display()),
            "-Clink-arg=--import-memory",
            "-Clink-arg=--strip-debug",
        ])
        .status()?;
    if !status.success() {
        return Err(format!("compile runtime artifact {name}").into());
    }
    let bytes = fs::read(
        target
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join(format!("{}.wasm", package.replace('-', "_"))),
    )?;
    wasmparser::Validator::new().validate_all(&bytes)?;
    fs::write(output.join(format!("{name}.wasm")), bytes)?;
    Ok(())
}

fn compile_json_codecs(runtime: &Path, output: &Path) -> Result<()> {
    let mut source = fs::read_to_string(runtime.join("json/src/decode.wat"))?;
    source = source.replace(
        "@write@",
        &fs::read_to_string(runtime.join("json/src/write.wat"))?,
    );
    macro_rules! constants {
        ($module:ident: $($name:ident),+ $(,)?) => { $(
            source = source.replace(concat!("@", stringify!($module), ".", stringify!($name), "@"),
                &layout::$module::$name.to_string());
        )+ };
    }
    constants!(descriptor: NUMBER, BOOLEAN, STRING, RECORD, ARRAY, EXCEPTION, SIGNED_INT,
        UNSIGNED_INT, JSON_VALUE, JSON_NUMERIC, JSON_TUPLE, JSON_VARIANT, JSON_VALUE_UNION,
        JSON_DICTIONARY, JSON_DATE, UNDEFINED_ALLOWED, NULL_ALLOWED, ABSENCE_MASK, SCHEMA_MASK, UNDEFINED_BITS,
        UNDEFINED_BOOL, COPY_UNDEFINED, COPY_ABSENT,
        COPY_NONFINITE, VARIANT_TAG, FIELDS, FIELD_SIZE, FIELD_SCHEMA, FIELD_OFFSET,
        DICTIONARY_VALUE_SCHEMA, DICTIONARY_VALUE_OFFSET, WRITE_NULL, WRITE_TRUE, WRITE_FALSE,
        WRITE_EMPTY_OBJECT, WRITE_COLON, WRITE_QUOTE, WRITE_TAG_KEY, WRITE_VALUE_KEY,
        WRITE_TAG_NAME, WRITE_RETAINS_JSON, WRITE_DATE_BUFFER, WRITE_DATE_CAPACITY);
    constants!(numeric: UNDEFINED, NULL, OMITTED, SIGNED, UNSIGNED);
    constants!(array: NUMERIC_ELEMENTS, SLOT, NUMERIC_SLOT, LEN, CAP, DATA);
    constants!(json: NEXT);
    constants!(record: JSON, HEADER, ORDER, PACKED_FIELDS);
    constants!(variant: TAG, OWNER, VALUE, JSON, RETAINED_SIZE);
    constants!(exception: TAG_STRING, TYPE_ERROR);
    for (name, entry) in [
        ("graph", "decode_graph"),
        ("cursor", "decode_cursor"),
        ("copy", "copy"),
    ] {
        let mut source = source.replace("@input.NAME@", entry);
        for (section, enabled) in [
            ("CURSOR", name == "cursor"),
            ("GRAPH", name == "graph"),
            ("NATIVE", name == "copy"),
            ("PARSED", name != "copy"),
            ("IMMEDIATE", name != "cursor"),
        ] {
            // WAT block comments nest, so disabled sections can contain other sections.
            // Named branch targets let the WAT parser resolve labels after selection.
            source = source
                .replace(
                    &format!("@input.{section}@"),
                    if enabled { "" } else { "(;" },
                )
                .replace(
                    &format!("@input.END_{section}@"),
                    if enabled { "" } else { ";)" },
                );
        }
        if source.contains('@') {
            return Err(format!("unresolved JSON codec constant in {name}").into());
        }
        let bytes = wat::parse_str(source)?;
        wasmparser::Validator::new().validate_all(&bytes)?;
        fs::write(output.join(format!("json-{name}.wasm")), bytes)?;
    }
    Ok(())
}
