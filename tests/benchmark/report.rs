use anyhow::{Context as _, Result};

#[derive(Default, serde::Serialize)]
pub(super) struct Row {
    pub(super) workload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) core_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) defined_functions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) helper_functions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) optimization: Option<std::collections::BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) initialization_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) invocation_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) warm_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) initial_memory: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) memory: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) counters: Option<Vec<u64>>,
    #[serde(skip)]
    pub(super) milliseconds: Option<f64>,
}

impl Row {
    pub(super) fn markdown(&self) -> String {
        let number = |value: Option<u64>| {
            value.map_or("–".into(), |v| {
                let digits = v.to_string();
                let mut out = String::new();
                for (i, c) in digits.chars().enumerate() {
                    if i > 0 && (digits.len() - i) % 3 == 0 {
                        out.push(',');
                    }
                    out.push(c);
                }
                out
            })
        };
        format!(
            "| {} | {} | {} | {} | {} |",
            self.workload,
            number(self.bytes.map(|b| b as u64)),
            number(self.invocation_fuel),
            number(self.memory.map(|b| b as u64)),
            self.milliseconds
                .map_or("–".into(), |ms| format!("{ms:.3}"))
        )
    }
}

/// One line per workload field whose recorded and current values differ.
pub(super) fn differences(recorded: &str, current: &str) -> Result<Vec<String>> {
    use serde_json::{Map, Value};
    use std::collections::{BTreeMap, BTreeSet};
    let index = |text: &str| -> Result<BTreeMap<String, Map<String, Value>>> {
        let rows: Vec<Map<String, Value>> = serde_json::from_str(text)?;
        rows.into_iter()
            .map(|row| {
                let workload = row.get("workload").and_then(Value::as_str);
                Ok((workload.context("row without workload")?.to_owned(), row))
            })
            .collect()
    };
    let (before, after) = (index(recorded)?, index(current)?);
    let mut lines = Vec::new();
    for workload in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let (old, new) = (before.get(workload), after.get(workload));
        let fields: BTreeSet<_> = old.into_iter().chain(new).flat_map(Map::keys).collect();
        for field in fields {
            let value = |row: Option<&Map<String, Value>>| {
                row.and_then(|row| row.get(field))
                    .map_or("–".into(), Value::to_string)
            };
            let (old, new) = (value(old), value(new));
            if old != new {
                lines.push(format!("{workload} {field}: {old} -> {new}"));
            }
        }
    }
    Ok(lines)
}
