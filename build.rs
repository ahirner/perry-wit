use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=rust-toolchain.toml");
    perry_rt_build::build(&manifest.join("runtime"), &output.join("runtime"))
}
