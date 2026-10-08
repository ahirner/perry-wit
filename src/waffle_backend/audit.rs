//! Verification that the WAFFLE compiler path has zero LLVM or inkwell dependencies.

use anyhow::{Result, ensure};

/// Verifies that the compiler dependency graph contains no LLVM or inkwell references.
pub(crate) fn audit_no_llvm(cargo_lock: &str) -> Result<()> {
    for line in cargo_lock.lines() {
        if line.starts_with("name = ") {
            let pkg_name = line
                .trim_start_matches("name = ")
                .trim_matches('"')
                .to_lowercase();
            // Disallow LLVM / inkwell compiler backend crates (e.g. llvm-sys, inkwell)
            let is_compiler_llvm = (pkg_name == "llvm"
                || pkg_name.starts_with("llvm-")
                || pkg_name.ends_with("-llvm")
                || pkg_name.contains("inkwell"))
                && !pkg_name.contains("gnullvm");
            ensure!(
                !is_compiler_llvm,
                "Prohibited LLVM/inkwell dependency detected in compiler graph: {pkg_name}"
            );
        }
    }
    Ok(())
}
