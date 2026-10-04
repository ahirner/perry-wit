//! Shared trailers and completion transport for incoming and outgoing HTTP.

use crate::waffle_backend::runtime::imports;

pub(super) fn functions() -> Vec<imports::Function> {
    [
        ("new-trailers", vec![], vec!["i64"]),
        ("write-trailers", vec!["i32"; 2], vec!["i32"]),
        ("read-trailers", vec!["i32"; 2], vec!["i32"]),
        ("drop-trailers-reader", vec!["i32"], vec![]),
        ("drop-trailers-writer", vec!["i32"], vec![]),
        ("new-completion", vec![], vec!["i64"]),
        ("write-completion", vec!["i32"; 2], vec!["i32"]),
        ("read-completion", vec!["i32"; 2], vec!["i32"]),
        ("drop-completion-reader", vec!["i32"], vec![]),
        ("drop-completion-writer", vec!["i32"], vec![]),
        ("new-set", vec![], vec!["i32"]),
        ("join", vec!["i32"; 2], vec![]),
        ("wait", vec!["i32"; 2], vec!["i32"]),
        ("drop-set", vec!["i32"], vec![]),
    ]
    .into_iter()
    .map(|(name, params, results)| imports::Function {
        name: name.into(),
        params,
        results,
    })
    .collect()
}
