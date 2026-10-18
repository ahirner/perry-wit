//! Line-oriented TypeScript emission shared by the generated aggregate and collection programs.
use std::fmt;

/// Weighted trace checksum emitted into generated programs; `NaN` items count as -55 so they
/// stay visible in the sum.
pub(crate) const CHECKSUM_TS: &str = "function checksum(seed: number, trace: number[]): number { let acc = seed; for (let i = 0; i < trace.length; i += 1) { const item = trace[i]; acc += (Number.isNaN(item) ? -55 : item + 0) * ((i % 5) + 1); } return acc; }";

/// [`CHECKSUM_TS`] for cases whose oracle is a Rust model; same operations in the same order.
pub(crate) fn checksum(seed: f64, trace: &[f64]) -> f64 {
    trace.iter().enumerate().fold(seed, |acc, (index, &item)| {
        let term = if item.is_nan() { -55.0 } else { item + 0.0 };
        acc + term * ((index % 5) as f64 + 1.0)
    })
}

/// Source under construction; every line is written at the current block depth.
#[derive(Default)]
pub(crate) struct Emitter {
    out: String,
    depth: usize,
}

impl Emitter {
    pub(crate) fn line(&mut self, text: fmt::Arguments<'_>) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        self.out.push_str(&fmt::format(text));
        self.out.push('\n');
    }

    /// Writes independent declarations in order, or reversed for the permuted metamorphic form.
    pub(crate) fn ordered(&mut self, items: impl IntoIterator<Item = String>, permuted: bool) {
        let mut items = items.into_iter().collect::<Vec<_>>();
        if permuted {
            items.reverse();
        }
        for item in items {
            self.line(format_args!("{item}"));
        }
    }

    /// Writes the lines `body` emits one level deeper.
    pub(crate) fn indented(&mut self, body: impl FnOnce(&mut Self)) {
        self.depth += 1;
        body(self);
        self.depth -= 1;
    }

    /// Writes `head {`, the indented `body`, and the closing brace.
    pub(crate) fn block(&mut self, head: fmt::Arguments<'_>, body: impl FnOnce(&mut Self)) {
        self.line(format_args!("{head} {{"));
        self.indented(body);
        self.line(format_args!("}}"));
    }

    pub(crate) fn finish(self) -> String {
        self.out
    }
}
