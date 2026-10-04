//! Shared trailers and completion transport for incoming and outgoing HTTP.

use crate::waffle_backend::component::forward;

pub(super) fn bindings(prefix: &str) -> String {
    format!(
        r#"
      (core func ${prefix}-new-trailers (canon future.new $http-trailers))
      (core func ${prefix}-write-trailers (canon future.write $http-trailers async (memory (core memory $guest "memory"))))
      (core func ${prefix}-read-trailers (canon future.read $http-trailers (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func ${prefix}-drop-trailers-reader (canon future.drop-readable $http-trailers))
      (core func ${prefix}-drop-trailers-writer (canon future.drop-writable $http-trailers))
      (core func ${prefix}-new-completion (canon future.new $http-completion))
      (core func ${prefix}-write-completion (canon future.write $http-completion async (memory (core memory $guest "memory"))))
      (core func ${prefix}-read-completion (canon future.read $http-completion (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func ${prefix}-drop-completion-reader (canon future.drop-readable $http-completion))
      (core func ${prefix}-drop-completion-writer (canon future.drop-writable $http-completion))
      (core func ${prefix}-new-set (canon waitable-set.new))
      (core func ${prefix}-join (canon waitable.join))
      (core func ${prefix}-wait (canon waitable-set.wait (memory (core memory $guest "memory"))))
      (core func ${prefix}-drop-set (canon waitable-set.drop))
    "#
    )
}

pub(super) fn functions(prefix: &str) -> Vec<forward::Function> {
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
    .map(|(name, params, results)| forward::Function {
        name: name.into(),
        params,
        results,
        target: format!("(func ${prefix}-{name})"),
    })
    .collect()
}
