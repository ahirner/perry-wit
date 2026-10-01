//! Conformance report generation and evidence aggregation.

use std::collections::HashMap;

use anyhow::Result;
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

/// Verification evidence for an individual test case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaseEvidence {
    pub case_path: String,
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
    pub timestamp: String,
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
    pub fn build(catalog: &CapabilityCatalog, results: &[ComparisonResult]) -> Self {
        let results_by_path: HashMap<String, &ComparisonResult> =
            results.iter().map(|r| (r.case_path.clone(), r)).collect();

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
            let mut all_passed = true;
            let mut has_case = false;

            for case_ref in &cap.conformance {
                if let Some(res) = results_by_path.get(case_ref) {
                    has_case = true;
                    if !res.matched {
                        all_passed = false;
                    }
                    case_evidences.push(CaseEvidence {
                        case_path: case_ref.clone(),
                        passed: res.matched,
                        discrepancies: res.discrepancies.clone(),
                    });
                } else {
                    has_case = false;
                    all_passed = false;
                    case_evidences.push(CaseEvidence {
                        case_path: case_ref.clone(),
                        passed: false,
                        discrepancies: vec!["Test case was not executed".to_string()],
                    });
                }
            }

            let status = if !has_case {
                missing_capabilities += 1;
                EvidenceStatus::Missing
            } else if all_passed {
                passing_capabilities += 1;
                EvidenceStatus::Passed
            } else {
                failing_capabilities += 1;
                EvidenceStatus::Failed
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

        Self {
            version: catalog.version.clone(),
            timestamp: "2026-10-01T22:30:00Z".to_string(),
            total_capabilities: catalog.capabilities.len(),
            supported_capabilities: supported_count,
            passing_capabilities,
            failing_capabilities,
            missing_capabilities,
            coverage_percent,
            evidence,
        }
    }

    /// Renders a human-readable markdown summary table.
    pub fn render_markdown_table(&self) -> String {
        let mut md = String::new();
        md.push_str("# Conformance Evaluation Report\n\n");
        md.push_str(&format!(
            "**Catalog Version:** {} | **Supported Capabilities:** {} | **Passing:** {} | **Coverage:** {:.1}%\n\n",
            self.version, self.supported_capabilities, self.passing_capabilities, self.coverage_percent
        ));
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
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                ev.capability_id, ev.capability_name, ev.tier, support_str, status_badge
            ));
        }

        md
    }

    /// Renders the full report as formatted JSON.
    pub fn render_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}
