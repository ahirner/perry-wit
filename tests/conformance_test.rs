//! Integration test for formal behavioral conformance and differential equivalence against Node.js oracle.

#[path = "../src/conformance/output.rs"]
mod output;

use std::fs;
use std::path::Path;

use perry_wit::conformance::{
    CapabilityCatalog, ConformanceReport, TestEvidence, run_conformance_suite,
};

#[test]
fn shell_banner_filter_preserves_component_output() {
    for banner in [
        "",
        "Perry-WIT compiler development: cargo build, cargo test\n",
        "Perry-WIT component SDK: tsc --noEmit, perry-wit, wasmtime\n",
        "=== Perry-WIT Hermetic Environment ===\nTools ready\n======================================\n",
    ] {
        let captured = format!("{banner}Perry-WIT application output\n42\n");
        let filtered = output::filter_nix_banner(&captured);
        assert_eq!(filtered, "Perry-WIT application output\n42");
        assert_eq!(
            output::normalize_stream(&filtered),
            ["Perry-WIT application output", "42"]
        );
    }
}

#[test]
fn test_differential_conformance_suite() {
    let cases_dir = Path::new("tests/conformance/cases");
    let scratch = tempfile::tempdir().unwrap();
    let scratch_dir = scratch.path();
    let results =
        run_conformance_suite(cases_dir, scratch_dir).expect("Running differential test suite");

    assert!(
        !results.is_empty(),
        "No conformance test cases were executed"
    );

    let mut failed_cases = Vec::new();
    for res in &results {
        println!("=== CASE: {} ===", res.case_path);
        println!("Matched: {}", res.matched);
        println!(
            "Oracle Exit: {}, SUT Exit: {}",
            res.oracle.exit_code, res.wasm.exit_code
        );
        if !res.discrepancies.is_empty() {
            println!("Discrepancies: {:?}", res.discrepancies);
        }
        if !res.matched {
            failed_cases.push(res);
        }
    }

    let report_status = if let Some(path) = std::env::var_os("PERRY_RUST_EVIDENCE") {
        let mut evidence: Vec<TestEvidence> =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        evidence.extend(results.iter().map(TestEvidence::from));
        let catalog = CapabilityCatalog::load_embedded().unwrap();
        let report = ConformanceReport::build(
            &catalog,
            &evidence,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        println!("{}", report.render_markdown_table());
        if let Some(report_path) = std::env::var_os("PERRY_CONFORMANCE_REPORT") {
            fs::write(report_path, report.render_json().unwrap()).unwrap();
        }
        Some(report.require_complete())
    } else {
        None
    };

    if let Some(status) = report_status {
        status.expect("All advertised capabilities need executed evidence");
    }

    if !failed_cases.is_empty() {
        let summary = failed_cases
            .iter()
            .map(|res| {
                format!(
                    "Case {} failed differential equivalence:\nDiscrepancies: {:?}\nNode stdout: {:?}\nWasm stdout: {:?}\nNode stderr: {:?}\nWasm stderr: {:?}",
                    res.case_path,
                    res.discrepancies,
                    res.oracle.stdout,
                    res.wasm.stdout,
                    res.oracle.stderr,
                    res.wasm.stderr
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        panic!(
            "{} conformance case(s) failed differential equivalence:\n\n{}",
            failed_cases.len(),
            summary
        );
    }
}
