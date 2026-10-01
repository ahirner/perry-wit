//! Integration test for formal behavioral conformance and differential equivalence against Node.js oracle.

use std::fs;
use std::path::Path;

use perry_wit::conformance::{
    CapabilityCatalog, ConformanceReport, EvidenceStatus, run_conformance_suite,
};

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
    assert_eq!(
        report.missing_capabilities, 0,
        "Expected 0 missing capabilities in report"
    );
    assert_eq!(
        report.coverage_percent, 100.0,
        "Expected 100% conformance coverage for supported capabilities"
    );

    let json_report = report.render_json().expect("Serializing report to JSON");
    assert!(json_report.contains("web.object_spread"));
    assert!(json_report.contains("web.console"));
    assert!(json_report.contains("web.promise_all"));
    assert!(json_report.contains("web.fetch"));
    assert!(json_report.contains("web.response_json"));
    assert!(json_report.contains("node.process_exit"));

    for ev in &report.evidence {
        if ev.status != EvidenceStatus::Unsupported {
            assert_eq!(
                ev.status,
                EvidenceStatus::Passed,
                "Capability {} failed conformance verification",
                ev.capability_id
            );
        }
    }
}
