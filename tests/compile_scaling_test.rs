//! TODOs.md P0.1: compile time must stay polynomial in expression nesting.
//! The legacy backend's type inference multiplied compile time by about 8 per extra `.c`;
//! the MIR builder types each expression once.
use perry_wit::compile_core;
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn property_chain(depth: usize) -> String {
    let mut source = String::new();
    for level in 0..depth {
        source.push_str(&format!(
            "interface N{level} {{ v: number; c: N{} }}\n",
            level + 1
        ));
    }
    source.push_str(&format!("interface N{depth} {{ v: number }}\n"));
    source.push_str(&format!(
        "function get(o: N0): number {{ return o{}.v; }}\n",
        ".c".repeat(depth)
    ));
    let mut literal = "{ v: 1 }".to_string();
    for level in (0..depth).rev() {
        literal = format!("{{ v: {level}, c: {literal} }}");
    }
    source.push_str(&format!(
        "export function run(n: number): number {{ return get({literal}) + n; }}\n"
    ));
    source
}

/// Compile on a worker thread so an exponential compile cannot hang the test binary.
fn compile_within(depth: usize, limit: Duration) -> Result<Duration, String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let started = Instant::now();
        let result = compile_core(&property_chain(depth), "chain.ts")
            .map(|_| started.elapsed())
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(limit)
        .map_err(|_| format!("depth {depth} did not compile within {limit:?}"))?
}

#[test]
fn shallow_property_chain_compiles() {
    compile_within(2, Duration::from_secs(60)).unwrap();
}

#[test]
fn deep_property_chain_compiles_quickly() {
    compile_within(32, Duration::from_secs(10)).unwrap();
}
