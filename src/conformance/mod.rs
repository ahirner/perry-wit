//! Formal conformance evaluation and capability contracts.

pub mod catalog;
mod output;
pub mod report;
pub mod runner;

pub use catalog::{Capability, CapabilityCatalog, SupportLevel, Tier};
pub use report::{CapabilityEvidence, CaseEvidence, ConformanceReport, EvidenceStatus};
pub use runner::{ComparisonResult, ExecutionVector, run_conformance_suite};
