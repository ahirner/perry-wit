use perry_wit::conformance::{CapabilityCatalog, EvidenceReference, SupportLevel};
use std::path::Path;

#[test]
fn test_embedded_catalog_loads_and_validates() {
    let catalog = CapabilityCatalog::load_embedded().expect("embedded catalog must be valid");
    assert_eq!(catalog.version, "0.1.0");
    assert!(!catalog.tiers.is_empty());
    assert!(!catalog.capabilities.is_empty());

    let supported = catalog.supported_capabilities();
    assert!(!supported.is_empty());

    for cap in supported {
        assert!(
            cap.support == SupportLevel::Full || cap.support == SupportLevel::Partial,
            "supported capability must be full or partial"
        );
        assert!(
            !cap.invariants.is_empty(),
            "capability '{}' must have invariants",
            cap.id
        );
        assert!(
            !cap.conformance.is_empty(),
            "capability '{}' must have conformance references",
            cap.id
        );
        for reference in &cap.conformance {
            let path = match EvidenceReference::parse(reference).unwrap() {
                EvidenceReference::Node { path } => path,
                EvidenceReference::Rust { target, .. } => target,
            };
            assert!(
                Path::new(path).is_file(),
                "capability '{}' references missing evidence: {reference}",
                cap.id
            );
        }
    }
}

#[test]
fn test_catalog_rejects_duplicates() {
    let duplicate_json = r#"{
        "version": "0.1.0",
        "description": "test",
        "tiers": [{"id": "t1", "name": "Tier 1", "description": "desc"}],
        "capabilities": [
            {
                "id": "cap.one",
                "name": "One",
                "tier": "t1",
                "support": "full",
                "domain": "test",
                "invariants": ["inv"],
                "differences": [],
                "conformance": ["node:test.ts"]
            },
            {
                "id": "cap.one",
                "name": "One Duplicate",
                "tier": "t1",
                "support": "full",
                "domain": "test",
                "invariants": ["inv"],
                "differences": [],
                "conformance": ["node:test.ts"]
            }
        ]
    }"#;

    assert!(CapabilityCatalog::parse(duplicate_json).is_err());
}
