use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[expect(dead_code, unreachable_pub)]
#[path = "crates/perry-abi/src/layout.rs"]
mod layout;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    embed_wasi(&manifest_dir, &out_dir);
    let helpers = out_dir.join("helpers");
    if helpers.exists() {
        fs::remove_dir_all(&helpers)?;
    }
    fs::create_dir_all(&helpers)?;
    compile_json_helpers(&manifest_dir, &helpers)?;

    for path in [
        "src/helpers",
        "crates/perry-abi",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=PERRY_HELPER_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=RUSTC");

    for helper in [
        "text",
        "normalization",
        "fetch",
        "json",
        "time",
        "number",
        "decimal",
    ] {
        compile_cargo_helper(
            &manifest_dir,
            &out_dir,
            &format!("perry-{helper}-helper"),
            &format!("crates/{helper}-helper"),
            helper,
        );
    }
    for (package, helper) in [
        ("perry-runtime", "runtime"),
        ("perry-std", "std"),
        ("perry-fs", "fs"),
        ("perry-web", "web"),
    ] {
        compile_cargo_helper(
            &manifest_dir,
            &out_dir,
            package,
            &format!("crates/{package}"),
            helper,
        );
    }
    compile_cargo_helper(
        &manifest_dir,
        &out_dir,
        "perry-runtime",
        "crates/perry-runtime",
        "runtime-collectible",
    );
    Ok(())
}

fn compile_json_helpers(manifest: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut source = fs::read_to_string(manifest.join("src/helpers/json/decode.wat"))?;
    source = source.replace(
        "@write@",
        &fs::read_to_string(manifest.join("src/helpers/json/write.wat"))?,
    );
    macro_rules! constants {
        ($module:ident: $($name:ident),+ $(,)?) => { $(
            source = source.replace(concat!("@", stringify!($module), ".", stringify!($name), "@"),
                &layout::$module::$name.to_string());
        )+ };
    }
    constants!(descriptor: NUMBER, BOOLEAN, STRING, RECORD, ARRAY, EXCEPTION, SIGNED_INT,
        UNSIGNED_INT, JSON_VALUE, JSON_NUMERIC, JSON_TUPLE, JSON_VARIANT, JSON_NUMBER_STRING,
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

fn embed_wasi(manifest: &Path, output: &Path) {
    println!("cargo:rerun-if-env-changed=WASI_WIT_PATH");
    let directory = env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("wit/deps"));
    let mut generated = String::from("pub(super) const PACKAGES: &[&[(&str, &str)]] = &[\n");
    for package in ["cli", "clocks", "filesystem", "http", "random", "sockets"] {
        let path = directory.join(package);
        println!("cargo:rerun-if-changed={}", path.display());
        let mut files = fs::read_dir(&path)
            .unwrap_or_else(|error| {
                panic!(
                    "read pinned WASI package {}: {error}; build inside nix develop",
                    path.display()
                )
            })
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wit"))
            .collect::<Vec<_>>();
        files.sort();
        generated.push_str("&[\n");
        for file in files {
            generated.push_str(&format!(
                "({:?}, {:?}),\n",
                file.file_name().unwrap().to_str().unwrap(),
                fs::read_to_string(&file).unwrap()
            ));
        }
        generated.push_str("],\n");
    }
    generated.push_str("];\n");
    fs::write(output.join("wasi_packages.rs"), generated).unwrap();
}

fn compile_cargo_helper(
    manifest: &Path,
    output: &Path,
    package: &str,
    crate_dir: &str,
    helper: &str,
) {
    println!("cargo:rerun-if-changed={crate_dir}");
    let mut target = env::var_os("PERRY_HELPER_TARGET_DIR")
        .map(|path| manifest.join(path))
        .unwrap_or_else(|| output.join("cargo-helper-build"));
    let collectible = helper == "runtime-collectible";
    if collectible {
        target.push("collectible");
    }
    let undefined = manifest.join("src/helpers/undefined.txt");
    let mut command = Command::new(env::var_os("CARGO").expect("Cargo sets CARGO"));
    command
        .current_dir(manifest)
        .env("CARGO_ENCODED_RUSTFLAGS", "-Crelocation-model=pic")
        .env_remove("RUSTFLAGS")
        // Embedded helpers are release builds; incremental compilation would change them.
        .env_remove("CARGO_INCREMENTAL")
        .args([
            "rustc",
            "--locked",
            "--offline",
            "--release",
            "--lib",
            &format!("--package={package}"),
            "--crate-type=cdylib",
            "--target=wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target);
    if collectible {
        command.args(["--features", "collectible"]);
    }
    // Keep bounded, allocation-light components small; optimize computation and the
    // collectible runtime for the loops and application work that need them.
    let optimization = if helper == "runtime" {
        "-Copt-level=z"
    } else {
        "-Copt-level=2"
    };
    let status = command
        .args([
            "--",
            optimization,
            "-Clto=fat",
            "-Clink-arg=--shared",
            "-Clink-arg=--no-entry",
            &format!("-Clink-arg=--allow-undefined-file={}", undefined.display()),
            "-Clink-arg=--import-memory",
            "-Clink-arg=--strip-debug",
        ])
        .status()
        .expect("build embedded Cargo helper");
    assert!(status.success(), "compile embedded {helper} helper");
    let source = target.join(format!(
        "wasm32-unknown-unknown/release/{}.wasm",
        package.replace('-', "_")
    ));
    let module = output.join("helpers").join(format!("{helper}.wasm"));
    fs::copy(source, &module).expect("copy embedded Cargo helper");
    let wasm = fs::read(module).expect("read embedded Cargo helper");
    wasmparser::Validator::new()
        .validate_all(&wasm)
        .expect("validate embedded Cargo helper");
}
