use std::{
    env, fs,
    path::{Path, PathBuf},
};
fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    embed_wasi(&manifest, &output);
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
