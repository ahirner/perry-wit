//! Seeded differential and metamorphic checks of the existing supported language.
#[path = "generative/model.rs"]
mod model;
#[path = "generative/runner.rs"]
mod runner;

use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use model::{Binary, Form, Generator, Number, Program};

#[test]
fn generated_programs_match_node() -> Result<()> {
    runner::campaign()
}

#[test]
fn missing_side_effect_is_detected_and_reduced() -> Result<()> {
    runner::verify_trace_fault()
}

#[test]
#[ignore = "isolated compiler/execution worker, invoked by generated_programs_match_node"]
fn generative_worker() -> Result<()> {
    runner::worker()
}

#[test]
fn generation_and_reduction_preserve_replay() -> Result<()> {
    for seed in 0..64 {
        let expression = Generator::with_apis(seed, 5).number(3);
        assert_eq!(expression, Generator::with_apis(seed, 5).number(3));
        let program = Program {
            expression,
            form: Form::Direct,
        };
        assert_eq!(
            program,
            serde_json::from_slice(&serde_json::to_vec(&program)?)?
        );
        for candidate in program.reductions() {
            assert!(candidate.source().len() < program.source().len());
        }
    }
    let original = Program {
        expression: Number::Mark(Box::new(Number::Binary(
            Binary::Add,
            Box::new(Number::Input),
            Box::new(Number::Integer(1)),
        ))),
        form: Form::Direct,
    };
    let (minimal, attempts) = runner::reduce(original.clone(), 100, |candidate| {
        Ok(matches!(candidate.expression, Number::Mark(_)))
    })?;
    assert_eq!(minimal.expression, Number::Mark(Box::new(Number::Input)));
    assert!(attempts > 0);
    let (unchanged, attempts) = runner::reduce(original.clone(), 0, |_| panic!("zero budget"))?;
    assert_eq!(unchanged, original);
    assert_eq!(attempts, 0);
    Ok(())
}

#[test]
fn observations_preserve_ieee_values_and_process_failures() -> Result<()> {
    assert_ne!(runner::number(0.0), runner::number(-0.0));
    assert_ne!(
        runner::number(f64::INFINITY),
        runner::number(f64::NEG_INFINITY)
    );
    assert_eq!(runner::number(f64::NAN), runner::number(-f64::NAN));
    let directory = tempfile::tempdir()?;
    let output = runner::command(
        Command::new("node").args(["-e", "process.exit(7)"]),
        directory.path(),
        Duration::from_secs(5),
    )?;
    assert!(
        matches!(output.status, runner::ProcessStatus::Exited(status) if status.code() == Some(7))
    );
    let output = runner::command(
        Command::new("node").args(["-e", "for (;;) {}"]),
        directory.path(),
        Duration::from_millis(100),
    )?;
    assert!(matches!(output.status, runner::ProcessStatus::TimedOut));
    Ok(())
}
