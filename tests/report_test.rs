use perry_wit::conformance::{
    CapabilityCatalog, ComparisonResult, ConformanceReport, EvidenceStatus, ExecutionVector,
};

#[test]
fn evidence_status_and_counters_do_not_depend_on_case_order() {
    let mut catalog = CapabilityCatalog::load_embedded().unwrap();
    catalog.capabilities.truncate(1);
    let execution = ExecutionVector {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
    };
    let result = ComparisonResult {
        case_path: "executed.ts".into(),
        matched: true,
        oracle: execution.clone(),
        wasm: execution,
        discrepancies: Vec::new(),
    };
    for (matched, expected) in [
        (true, EvidenceStatus::Missing),
        (false, EvidenceStatus::Failed),
    ] {
        for refs in [["missing.ts", "executed.ts"], ["executed.ts", "missing.ts"]] {
            catalog.capabilities[0].conformance = refs.into_iter().map(String::from).collect();
            let report = ConformanceReport::build(
                &catalog,
                &[ComparisonResult {
                    matched,
                    ..result.clone()
                }],
            );
            assert_eq!(report.evidence[0].status, expected);
            assert_eq!(report.passing_capabilities, 0);
            assert_eq!(report.missing_capabilities, usize::from(matched));
            assert_eq!(report.failing_capabilities, usize::from(!matched));
        }
    }
    for refs in [Vec::new(), vec!["missing.ts"]] {
        catalog.capabilities[0].conformance = refs.into_iter().map(String::from).collect();
        assert_eq!(
            ConformanceReport::build(&catalog, &[]).evidence[0].status,
            EvidenceStatus::Missing
        );
    }
    catalog.capabilities[0].conformance = vec!["executed.ts".into()];
    let report = ConformanceReport::build(&catalog, &[result]);
    assert_eq!(report.evidence[0].status, EvidenceStatus::Passed);
    assert_eq!(report.passing_capabilities, 1);
}
