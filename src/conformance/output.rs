//! Pure output normalization for differential conformance comparisons.

/// Removes known Nix shell messages while preserving component output.
pub(crate) fn filter_nix_banner(s: &str) -> String {
    let mut out = Vec::new();
    let mut in_banner = false;
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed == "=== Perry-WIT Hermetic Environment ===" {
            in_banner = true;
            continue;
        }
        if in_banner {
            if trimmed.contains("======================================") {
                in_banner = false;
            }
            continue;
        }
        if matches!(
            trimmed,
            "Perry-WIT compiler development: cargo build, cargo test"
                | "Perry-WIT component SDK: tsc --noEmit, perry-wit, wasmtime"
        ) || trimmed.starts_with("warning:")
            || trimmed.starts_with("building '/nix/store")
            || trimmed.starts_with("evaluating flake")
            || trimmed.starts_with("copying path '/nix/store")
        {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// Compares stream content without incidental whitespace or blank lines.
pub(crate) fn normalize_stream(s: &str) -> Vec<String> {
    s.lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}
