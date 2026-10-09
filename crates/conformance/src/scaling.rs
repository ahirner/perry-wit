//! Cost-growth contracts: guest fuel must grow at most proportionally with input size.
//!
//! Each case calls the same instance with a warm-up size, a base size, and `factor` times the
//! base size. Values and traces must equal Node's; the fuel ratio between the last two calls
//! must not exceed `max_growth`. Fuel is deterministic, so the bound is not timing-sensitive.
use crate::{
    execution::{Evidence, Outcome},
    registry::{Case, Contract, Witness},
};
use anyhow::{Context, Result, ensure};
use proptest::prelude::*;

const FACTOR: u32 = 4;
/// Linear work yields 4; allow constant-factor noise and n·log n, reject quadratic (16).
const MAX_GROWTH: u32 = 6;

pub fn inputs(base: u32, factor: u32) -> [f64; 3] {
    [f64::from(base), f64::from(base), f64::from(base * factor)]
}

fn workload(name: &str) -> &'static str {
    match name {
        "retained-records" => {
            r#"
interface Point { x: number; y: number }
export function run(x: number): { value: number; trace: number[] } {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
    items.push({ x: i, y: i * 2 });
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    const point = items[k];
    sum = (sum + point.x + point.y) % 1000003;
  }
  return { value: sum, trace: [items.length] };
}
"#
        }
        "number-push" => {
            r#"
export function run(x: number): { value: number; trace: number[] } {
  const items: number[] = [];
  for (let i = 0; i < x; i++) {
    items.push(i * 3);
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    sum = (sum + items[k]) % 1000003;
  }
  return { value: sum, trace: [items.length] };
}
"#
        }
        "temporary-records" => {
            r#"
interface Point { x: number; y: number }
function make(i: number): Point {
  return { x: i, y: i * 2 };
}
export function run(x: number): { value: number; trace: number[] } {
  let sum = 0;
  for (let i = 0; i < x; i++) {
    const point = make(i);
    sum = (sum + point.x + point.y) % 1000003;
  }
  return { value: sum, trace: [] };
}
"#
        }
        "string-append" => {
            r#"
export function run(x: number): { value: number; trace: number[] } {
  let text = "";
  for (let i = 0; i < x; i++) {
    text = text + "x";
  }
  return { value: text.length, trace: [] };
}
"#
        }
        _ => unreachable!("unknown scaling workload {name}"),
    }
}

fn case(name: &str, base: u32) -> Case {
    Case::Scaling {
        source: workload(name).into(),
        base,
        factor: FACTOR,
        max_growth: MAX_GROWTH,
    }
}

pub fn contracts() -> Vec<Contract> {
    [
        (
            "wasi.execution.scaling.retained_records",
            "retained-records",
            "Pushing n records into a local array and reading them back costs O(n) guest fuel",
        ),
        (
            "wasi.execution.scaling.number_push",
            "number-push",
            "Pushing n numbers into a dense array and summing them costs O(n) guest fuel",
        ),
        (
            "wasi.execution.scaling.temporary_records",
            "temporary-records",
            "One temporary record per loop iteration costs O(n) guest fuel in total",
        ),
        (
            "wasi.execution.scaling.string_append",
            "string-append",
            "Appending one character n times costs at most n·log n guest fuel",
        ),
    ]
    .into_iter()
    .map(|(id, name, description)| Contract {
        id,
        description,
        specification: "TODOs.md P0.1 (implementation cost bound, not an ECMAScript requirement)",
        domain: "Warm-up n, n, and 4n calls on one instance; n in 32..=128 generated, 128 directed; values equal Node, fuel ratio of the last two calls at most 6",
        witnesses: vec![Witness {
            partition: "n-128",
            case: case(name, 128),
        }],
        strategy: (32u32..=128).prop_map(move |base| case(name, base)).boxed(),
        check: check_scaling,
    })
    .collect()
}

pub fn check_scaling(case: &Case, evidence: &Evidence) -> Result<()> {
    let Case::Scaling {
        factor, max_growth, ..
    } = case
    else {
        anyhow::bail!("scaling checker requires a scaling case")
    };
    let (
        Outcome::Values {
            observations: expected,
            ..
        },
        Outcome::Values {
            observations: actual,
            fuel_used,
            ..
        },
    ) = (&evidence.oracle, &evidence.component)
    else {
        anyhow::bail!("execution did not complete within limits: {evidence:?}")
    };
    ensure!(expected == actual, "value or trace mismatch: {evidence:?}");
    ensure!(
        actual.len() == 3 && fuel_used.len() == 3,
        "missing scaling observations"
    );
    let base = fuel_used[1].max(1);
    let scaled = fuel_used[2];
    // Compare scaled/base against max_growth with exact integer arithmetic.
    ensure!(
        u128::from(scaled) <= u128::from(base) * u128::from(*max_growth),
        "fuel grew {:.2}x for a {factor}x larger input (limit {max_growth}x): {fuel_used:?}",
        scaled as f64 / base as f64
    );
    Ok(())
}

pub fn override_inputs(case: &Case, directory: &std::path::Path) -> Result<()> {
    if let Case::Scaling { base, factor, .. } = case {
        std::fs::write(
            directory.join("inputs.json"),
            serde_json::to_vec(&inputs(*base, *factor).map(crate::execution::number))
                .context("scaling inputs")?,
        )?;
    }
    Ok(())
}
