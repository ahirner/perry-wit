//! Integration test for formal behavioral conformance and differential equivalence against Node.js oracle.

#[path = "../src/conformance/output.rs"]
mod output;

use std::fs;
use std::path::Path;

use perry_wit::conformance::{
    CapabilityCatalog, ConformanceReport, EvidenceStatus, run_conformance_suite,
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
    let scratch_dir = std::env::temp_dir().join("perry_wit_conformance");
    let _ = fs::create_dir_all(&scratch_dir);

    let catalog = CapabilityCatalog::load_embedded().expect("Loading embedded capability catalog");
    let results =
        run_conformance_suite(cases_dir, &scratch_dir).expect("Running differential test suite");

    assert!(
        !results.is_empty(),
        "No conformance test cases were executed"
    );

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
        assert!(
            res.matched,
            "Case {} failed differential equivalence:\nDiscrepancies: {:?}\nNode stdout: {:?}\nWasm stdout: {:?}\nNode stderr: {:?}\nWasm stderr: {:?}",
            res.case_path,
            res.discrepancies,
            res.oracle.stdout,
            res.wasm.stdout,
            res.oracle.stderr,
            res.wasm.stderr
        );
    }

    let report = ConformanceReport::build(&catalog, &results);

    println!("\n{}", report.render_markdown_table());

    assert_eq!(
        report.failing_capabilities, 0,
        "Expected 0 failing capabilities in report"
    );
    // Cargo verifies these suites separately; this runner only executes .ts cases.
    let integration_required: Vec<_> = catalog
        .supported_capabilities()
        .into_iter()
        .filter(|capability| {
            !capability.conformance.is_empty()
                && capability.conformance.iter().any(|reference| {
                    Path::new(reference)
                        .extension()
                        .is_some_and(|ext| ext == "rs")
                })
        })
        .map(|capability| capability.id.as_str())
        .collect();
    assert_eq!(report.missing_capabilities, integration_required.len());
    assert_eq!(
        report.passing_capabilities,
        report.supported_capabilities - integration_required.len()
    );

    let json_report = report.render_json().expect("Serializing report to JSON");
    assert!(json_report.contains("compiler.p3_json"));
    assert!(json_report.contains("compiler.p3_stdio"));
    assert!(json_report.contains("web.promise_all"));
    assert!(json_report.contains("web.fetch"));
    assert!(json_report.contains("compiler.p3_context"));
    assert!(json_report.contains("node.process_exit"));
    assert!(json_report.contains("compiler.p3_context"));

    for ev in &report.evidence {
        if ev.status != EvidenceStatus::Unsupported {
            let expected = if integration_required.contains(&ev.capability_id.as_str()) {
                EvidenceStatus::Missing
            } else {
                EvidenceStatus::Passed
            };
            assert_eq!(
                ev.status, expected,
                "Capability {} failed conformance verification",
                ev.capability_id
            );
        }
    }
}
