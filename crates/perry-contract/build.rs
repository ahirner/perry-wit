use std::{env, fs, io, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is missing")?)
            .join("../..");
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is missing")?);
    println!("cargo:rerun-if-env-changed=WASI_WIT_PATH");
    let directory = env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("wit/deps"));
    let mut generated = String::from("pub(super) const PACKAGES: &[&[(&str, &str)]] = &[\n");
    for package in ["cli", "clocks", "filesystem", "http", "random", "sockets"] {
        let path = directory.join(package);
        println!("cargo:rerun-if-changed={}", path.display());
        let entries = fs::read_dir(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "read pinned WASI package {}: {error}; build inside nix develop",
                    path.display()
                ),
            )
        })?;
        let mut files = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "wit") {
                files.push(path);
            }
        }
        files.sort();
        generated.push_str("&[\n");
        for file in files {
            generated.push_str(&format!(
                "({:?}, {:?}),\n",
                file.file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| format!("WIT filename is not UTF-8: {}", file.display()))?,
                fs::read_to_string(&file)?
            ));
        }
        generated.push_str("],\n");
    }
    generated.push_str("];\n");
    fs::write(output.join("wasi_packages.rs"), generated)?;
    Ok(())
}
