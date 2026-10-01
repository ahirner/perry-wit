use perry_wit::conformance::{CapabilityCatalog, SupportLevel};

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
                "conformance": ["test.ts"]
            },
            {
                "id": "cap.one",
                "name": "One Duplicate",
                "tier": "t1",
                "support": "full",
                "domain": "test",
                "invariants": ["inv"],
                "differences": [],
                "conformance": ["test.ts"]
            }
        ]
    }"#;

    assert!(CapabilityCatalog::parse(duplicate_json).is_err());
}
