use perry_wit::conformance::{
    CapabilityCatalog, ConformanceReport, EvidenceStatus, TestEvidence, TestOutcome,
};

#[test]
fn evidence_status_and_completion_gate_do_not_depend_on_case_order() {
    let mut catalog = CapabilityCatalog::load_embedded().unwrap();
    catalog.capabilities.truncate(1);
    let executed = "node:executed.ts";
    let missing = "rust:tests/missing.rs#unexecuted";
    for (outcome, expected) in [
        (TestOutcome::Passed, EvidenceStatus::Missing),
        (TestOutcome::Failed, EvidenceStatus::Failed),
        (TestOutcome::Skipped, EvidenceStatus::Missing),
    ] {
        for references in [[missing, executed], [executed, missing]] {
            catalog.capabilities[0].conformance =
                references.into_iter().map(String::from).collect();
            let report = ConformanceReport::build(
                &catalog,
                &[TestEvidence {
                    id: executed.into(),
                    outcome,
                    details: vec![],
                }],
                123,
            )
            .unwrap();
            assert_eq!(report.evidence[0].status, expected);
            assert_eq!(report.passing_capabilities, 0);
            assert!(report.require_complete().is_err());
        }
    }
    let evidence: Vec<_> = [executed, missing]
        .map(|id| TestEvidence {
            id: id.into(),
            outcome: TestOutcome::Passed,
            details: vec![],
        })
        .into();
    let report = ConformanceReport::build(&catalog, &evidence, 123).unwrap();
    assert_eq!(report.passing_capabilities, 1);
    report.require_complete().unwrap();
    assert!(report.generated_at_unix_seconds > 0);
    assert!(
        ConformanceReport::build(&catalog, &[evidence[0].clone(), evidence[0].clone()], 123)
            .is_err()
    );
}

#[test]
fn skipped_or_absent_evidence_cannot_complete_an_advertised_capability() {
    let catalog = CapabilityCatalog::load_embedded().unwrap();
    let skipped = catalog
        .capabilities
        .iter()
        .flat_map(|capability| &capability.conformance)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|id| TestEvidence {
            id: id.clone(),
            outcome: TestOutcome::Skipped,
            details: vec!["ignored test".into()],
        })
        .collect::<Vec<_>>();
    for results in [&[][..], skipped.as_slice()] {
        let report = ConformanceReport::build(&catalog, results, 123).unwrap();
        assert_eq!(report.missing_capabilities, report.supported_capabilities);
        assert!(report.require_complete().is_err());
    }
}
