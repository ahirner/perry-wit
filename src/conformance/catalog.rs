//! Machine-readable capability catalog parsing and validation.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// The embedded capability catalog JSON.
pub const CATALOG_JSON: &str = include_str!("../../catalog/capabilities.json");

/// Support level of a capability within the AOT WebAssembly compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupportLevel {
    Full,
    Partial,
    Unsupported,
}

/// A tier categorizing related capabilities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tier {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// A formal capability declared by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capability {
    pub id: String,
    pub name: String,
    pub tier: String,
    pub support: SupportLevel,
    pub domain: String,
    pub invariants: Vec<String>,
    pub differences: Vec<String>,
    pub conformance: Vec<String>,
}

/// The root capability catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityCatalog {
    pub version: String,
    pub description: String,
    pub tiers: Vec<Tier>,
    pub capabilities: Vec<Capability>,
}

impl CapabilityCatalog {
    /// Loads the embedded capability catalog.
    pub fn load_embedded() -> Result<Self> {
        let catalog: Self = serde_json::from_str(CATALOG_JSON)
            .context("failed to parse embedded capabilities.json")?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Parses and validates a capability catalog from a JSON string.
    pub fn parse(json_str: &str) -> Result<Self> {
        let catalog: Self =
            serde_json::from_str(json_str).context("failed to parse capabilities JSON string")?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Validates the catalog for internal consistency:
    /// - Non-empty version and description
    /// - Unique tier IDs
    /// - Unique capability IDs
    /// - All capabilities reference an existing tier
    /// - Supported capabilities (`full` or `partial`) must have at least one invariant
    /// - Supported capabilities must have at least one conformance test reference
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.version.is_empty(),
            "catalog version must not be empty"
        );
        ensure!(
            !self.description.is_empty(),
            "catalog description must not be empty"
        );

        let mut tier_ids = HashSet::new();
        for tier in &self.tiers {
            ensure!(!tier.id.is_empty(), "tier ID must not be empty");
            if !tier_ids.insert(&tier.id) {
                bail!("duplicate tier ID '{}'", tier.id);
            }
        }

        let mut cap_ids = HashSet::new();
        for cap in &self.capabilities {
            ensure!(!cap.id.is_empty(), "capability ID must not be empty");
            if !cap_ids.insert(&cap.id) {
                bail!("duplicate capability ID '{}'", cap.id);
            }

            if !tier_ids.contains(&cap.tier) {
                bail!(
                    "capability '{}' references non-existent tier '{}'",
                    cap.id,
                    cap.tier
                );
            }

            if cap.support != SupportLevel::Unsupported {
                ensure!(
                    !cap.invariants.is_empty(),
                    "supported capability '{}' must declare at least one invariant",
                    cap.id
                );
                ensure!(
                    !cap.conformance.is_empty(),
                    "supported capability '{}' must declare at least one conformance reference",
                    cap.id
                );
            }
        }

        Ok(())
    }

    /// Returns all supported capabilities (support level `full` or `partial`).
    pub fn supported_capabilities(&self) -> Vec<&Capability> {
        self.capabilities
            .iter()
            .filter(|c| c.support != SupportLevel::Unsupported)
            .collect()
    }

    /// Look up a capability by its unique ID.
    pub fn get_capability(&self, id: &str) -> Option<&Capability> {
        self.capabilities.iter().find(|c| c.id == id)
    }
}
