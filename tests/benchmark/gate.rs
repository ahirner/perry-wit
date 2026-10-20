//! Frozen workload comparison; measurement and baseline values stay in the parent.
use super::Row;
use anyhow::{Result, ensure};

pub(super) fn compare_baseline(baseline: &[Row], current: &[Row]) -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    let previous: BTreeMap<_, _> = baseline
        .iter()
        .map(|row| (row.workload.as_str(), row))
        .collect();
    let names: BTreeSet<_> = current.iter().map(|row| row.workload.as_str()).collect();
    ensure!(
        previous.keys().copied().eq(names.iter().copied()),
        "benchmark workload set changed"
    );
    ensure!(
        previous.len() == baseline.len() && names.len() == current.len(),
        "duplicate workload"
    );
    let mut failures = Vec::new();
    for row in current {
        let old = previous[row.workload.as_str()];
        if let Some(error) = &old.compilation_error {
            ensure!(
                row.compilation_error
                    .as_ref()
                    .is_none_or(|current| current == error),
                "{}: changed compilation error",
                row.workload
            );
            continue;
        }
        ensure!(
            row.compilation_error.is_none(),
            "{}: compilation regressed",
            row.workload
        );
        for (name, before, after) in [
            (
                "artifact bytes",
                old.bytes.map(|n| n as u64),
                row.bytes.map(|n| n as u64),
            ),
            (
                "core bytes",
                old.core_bytes.map(|n| n as u64),
                row.core_bytes.map(|n| n as u64),
            ),
            ("fuel", old.fuel, row.fuel),
            (
                "initial memory",
                old.initial_memory.map(|n| n as u64),
                row.initial_memory.map(|n| n as u64),
            ),
            (
                "peak memory",
                old.memory.map(|n| n as u64),
                row.memory.map(|n| n as u64),
            ),
        ]
        .into_iter()
        .chain(
            [
                (
                    "initialization fuel",
                    old.initialization_fuel,
                    row.initialization_fuel,
                ),
                ("invocation fuel", old.invocation_fuel, row.invocation_fuel),
            ]
            .into_iter()
            .filter(|(_, before, _)| before.is_some()),
        ) {
            match (before, after) {
                (Some(before), Some(after))
                    if u128::from(after) * 100 <= u128::from(before) * 105 => {}
                (None, None) => {}
                _ => failures.push(format!("{}: {name}: {before:?} -> {after:?}", row.workload)),
            }
        }
    }
    ensure!(
        failures.is_empty(),
        "benchmark regressions (limit 105%):\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn benchmark_gate_checks_each_metric_and_zero_baselines() {
    super::verify_baseline_inputs().unwrap();
    let sample = |bytes, fuel| Row {
        workload: "sample".into(),
        bytes: Some(bytes),
        fuel: Some(fuel),
        ..Row::default()
    };
    assert!(compare_baseline(&[sample(100, 0)], &[sample(105, 0)]).is_ok());
    assert!(compare_baseline(&[sample(100, 0)], &[sample(106, 0)]).is_err());
    assert!(compare_baseline(&[sample(100, 0)], &[sample(90, 1)]).is_err());
    assert!(compare_baseline(&[sample(100, 100)], &[sample(1, 106)]).is_err());
    assert!(compare_baseline(&[sample(100, 0)], &[]).is_err());

    let detailed = |initialization_fuel, invocation_fuel| Row {
        initialization_fuel,
        invocation_fuel,
        ..sample(100, 0)
    };
    for (initial_before, call_before, initial_after, call_after, accepted) in [
        (None, None, Some(20), Some(105), true),
        (Some(0), Some(100), Some(0), Some(105), true),
        (Some(0), None, Some(1), None, false),
        (None, Some(100), None, Some(106), false),
        (Some(0), None, None, None, false),
    ] {
        assert_eq!(
            compare_baseline(
                &[detailed(initial_before, call_before)],
                &[detailed(initial_after, call_after)],
            )
            .is_ok(),
            accepted,
        );
    }
}
