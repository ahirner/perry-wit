//! Conformance report generation and evidence aggregation.

use std::collections::HashMap;
use std::fmt::Write;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::catalog::{CapabilityCatalog, SupportLevel};
use super::runner::ComparisonResult;

/// Outcome status for capability conformance evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Passed,
    Failed,
    Missing,
    Unsupported,
}

/// A result observed by an executable test runner; skipped tests are not proof.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestEvidence {
    pub id: String,
    pub outcome: TestOutcome,
    #[serde(default)]
    pub details: Vec<String>,
}

impl From<&ComparisonResult> for TestEvidence {
    fn from(result: &ComparisonResult) -> Self {
        Self {
            id: format!("node:{}", result.case_path),
            outcome: if result.matched {
                TestOutcome::Passed
            } else {
                TestOutcome::Failed
            },
            details: result.discrepancies.clone(),
        }
    }
}

/// Verification evidence for an individual test case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaseEvidence {
    pub test_id: String,
    pub passed: bool,
    pub discrepancies: Vec<String>,
}

/// Verification evidence for a declared capability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityEvidence {
    pub capability_id: String,
    pub capability_name: String,
    pub tier: String,
    pub support: SupportLevel,
    pub cases: Vec<CaseEvidence>,
    pub status: EvidenceStatus,
}

/// Aggregate conformance evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConformanceReport {
    pub version: String,
    pub generated_at_unix_seconds: u64,
    pub total_capabilities: usize,
    pub supported_capabilities: usize,
    pub passing_capabilities: usize,
    pub failing_capabilities: usize,
    pub missing_capabilities: usize,
    pub coverage_percent: f64,
    pub evidence: Vec<CapabilityEvidence>,
}

impl ConformanceReport {
    /// Builds an aggregate report by cross-referencing catalog declarations against executed test results.
    pub fn build(
        catalog: &CapabilityCatalog,
        results: &[TestEvidence],
        generated_at_unix_seconds: u64,
    ) -> Result<Self> {
        let mut results_by_path = HashMap::new();
        for result in results {
            ensure!(
                results_by_path.insert(result.id.as_str(), result).is_none(),
                "duplicate execution evidence: {}",
                result.id
            );
        }

        let mut evidence = Vec::new();
        let mut passing_capabilities = 0;
        let mut failing_capabilities = 0;
        let mut missing_capabilities = 0;

        let supported = catalog.supported_capabilities();
        let supported_count = supported.len();

        for cap in &catalog.capabilities {
            if cap.support == SupportLevel::Unsupported {
                evidence.push(CapabilityEvidence {
                    capability_id: cap.id.clone(),
                    capability_name: cap.name.clone(),
                    tier: cap.tier.clone(),
                    support: cap.support,
                    cases: Vec::new(),
                    status: EvidenceStatus::Unsupported,
                });
                continue;
            }

            let mut case_evidences = Vec::new();
            let mut has_failed = false;
            let mut has_missing = cap.conformance.is_empty();

            for case_ref in &cap.conformance {
                if let Some(res) = results_by_path.get(case_ref.as_str()) {
                    has_failed |= res.outcome == TestOutcome::Failed;
                    has_missing |= res.outcome == TestOutcome::Skipped;
                    case_evidences.push(CaseEvidence {
                        test_id: case_ref.clone(),
                        passed: res.outcome == TestOutcome::Passed,
                        discrepancies: res.details.clone(),
                    });
                } else {
                    has_missing = true;
                    case_evidences.push(CaseEvidence {
                        test_id: case_ref.clone(),
                        passed: false,
                        discrepancies: vec!["Test case was not executed".to_string()],
                    });
                }
            }

            let status = if has_failed {
                failing_capabilities += 1;
                EvidenceStatus::Failed
            } else if has_missing {
                missing_capabilities += 1;
                EvidenceStatus::Missing
            } else {
                passing_capabilities += 1;
                EvidenceStatus::Passed
            };

            evidence.push(CapabilityEvidence {
                capability_id: cap.id.clone(),
                capability_name: cap.name.clone(),
                tier: cap.tier.clone(),
                support: cap.support,
                cases: case_evidences,
                status,
            });
        }

        let coverage_percent = if supported_count > 0 {
            (passing_capabilities as f64 / supported_count as f64) * 100.0
        } else {
            100.0
        };

        Ok(Self {
            version: catalog.version.clone(),
            generated_at_unix_seconds,
            total_capabilities: catalog.capabilities.len(),
            supported_capabilities: supported_count,
            passing_capabilities,
            failing_capabilities,
            missing_capabilities,
            coverage_percent,
            evidence,
        })
    }

    pub fn require_complete(&self) -> Result<()> {
        let unverified: Vec<_> = self
            .evidence
            .iter()
            .filter(|capability| {
                matches!(
                    capability.status,
                    EvidenceStatus::Failed | EvidenceStatus::Missing
                )
            })
            .map(|capability| capability.capability_id.as_str())
            .collect();
        ensure!(
            unverified.is_empty(),
            "unverified capability evidence: {}",
            unverified.join(", ")
        );
        Ok(())
    }

    /// Renders a human-readable markdown summary table.
    pub fn render_markdown_table(&self) -> String {
        let mut md = String::new();
        md.push_str("# Conformance Evaluation Report\n\n");
        write!(md,
            "**Catalog Version:** {} | **Supported Capabilities:** {} | **Passing:** {} | **Coverage:** {:.1}%\n\n",
            self.version, self.supported_capabilities, self.passing_capabilities, self.coverage_percent
        ).unwrap();
        md.push_str("| Capability ID | Name | Tier | Support | Status |\n");
        md.push_str("| :--- | :--- | :--- | :--- | :--- |\n");

        for ev in &self.evidence {
            let status_badge = match ev.status {
                EvidenceStatus::Passed => "PASS",
                EvidenceStatus::Failed => "FAIL",
                EvidenceStatus::Missing => "MISSING",
                EvidenceStatus::Unsupported => "N/A",
            };
            let support_str = match ev.support {
                SupportLevel::Full => "full",
                SupportLevel::Partial => "partial",
                SupportLevel::Unsupported => "unsupported",
            };
            writeln!(
                md,
                "| `{}` | {} | {} | {} | {} |",
                ev.capability_id, ev.capability_name, ev.tier, support_str, status_badge
            )
            .unwrap();
        }

        md
    }

    /// Renders the full report as formatted JSON.
    pub fn render_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}
