use anyhow::{Result, ensure};
use perry_conformance::{
    execution::{Executor, Fault, Limits, check_fault, command, number},
    program::{Form, Number, Program},
    registry::{self, Case},
    runner::{self, ConcreteCase, SavedCase, Settings},
    transitions::{self, TimerModel},
};
use proptest::{
    prelude::*,
    test_runner::{Config, RngSeed, TestCaseError, TestError, TestRunner},
};
use proptest_state_machine::ReferenceStateMachine;
use std::{fs, path::PathBuf, process::Command, time::Duration};

#[test]
fn registry_owns_regressions_and_partition_witnesses() -> Result<()> {
    let contracts = registry::contracts(3)?;
    let cases = contracts
        .iter()
        .flat_map(|c| &c.witnesses)
        .collect::<Vec<_>>();
    for name in [
        "branch-true-array",
        "branch-false-array",
        "first-array-index",
        "array-effects",
        "byte-input",
        "byte-effects",
        "nested-codecs",
        "stored-json-unicode",
        "direct-json-unicode",
    ] {
        let program: Program = serde_json::from_slice(&fs::read(format!(
            "{}/cases/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))?)?;
        ensure!(
            cases
                .iter()
                .any(|w| w.case == Case::Program(program.clone())),
            "unregistered regression {name}"
        );
    }
    for contract in contracts {
        assert_eq!(
            contract.catalog().partitions.len(),
            contract.witnesses.len()
        );
        assert!(
            contract.id.starts_with("ecma.")
                || contract.id.starts_with("web.")
                || contract.id.starts_with("node.")
                || contract.id.starts_with("wasi.")
        );
    }
    Ok(())
}

#[test]
fn state_machine_shrinking_preserves_valid_serializable_traces() {
    let mut runner = TestRunner::new(Config {
        cases: 64,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(
            &TimerModel::sequential_strategy(1..=20),
            |(_, actions, _)| {
                let actions = transitions::complete_trace(actions);
                let decoded: Vec<transitions::Action> =
                    serde_json::from_slice(&serde_json::to_vec(&actions).unwrap()).unwrap();
                prop_assert_eq!(&decoded, &actions);
                prop_assert!(!transitions::source(&actions).is_empty());
                prop_assert_eq!(transitions::expected_trace(&actions).len(), actions.len());
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn injected_faults_shrink_and_replay_through_the_execution_adapter() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_perry-conformance"));
    let executor = Executor::new(executable.clone(), Limits::default());
    for (id, fault) in [
        ("wasi.execution.detect_value_fault", Fault::Value),
        ("wasi.execution.detect_trace_fault", Fault::Trace),
        ("wasi.execution.detect_resource_fault", Fault::Resource),
    ] {
        let strategy = (0i64..32).prop_map(move |n| Case::Fault {
            program: Program {
                expression: Number::Mark(Box::new(Number::Integer(n))),
                form: Form::Direct,
            },
            fault,
        });
        let mut runner = TestRunner::new(Config {
            cases: 1,
            max_shrink_iters: 32,
            rng_seed: RngSeed::Fixed(42),
            failure_persistence: None,
            ..Config::default()
        });
        let result = runner.run(&strategy, |case| {
            let artifacts = tempfile::tempdir_in(directory.path()).unwrap();
            for evidence in executor.check(&case, artifacts.path()).unwrap() {
                check_fault(&case, &evidence).unwrap();
            }
            Err(TestCaseError::fail("detected injected fault"))
        });
        let Err(TestError::Fail(_, case)) = result else {
            panic!("fault was not minimized")
        };
        assert!(
            matches!(&case,Case::Fault{program:Program{expression:Number::Mark(n),..},..} if **n==Number::Integer(0))
        );
        let replay = directory.path().join("minimal.json");
        fs::write(
            &replay,
            serde_json::to_vec(&SavedCase {
                contract: id.into(),
                source: format!(
                    "// Saved concrete source survives emitter changes\n{}",
                    case.source()
                ),
                case,
                metamorphic: vec![],
                settings: Settings::default(),
                source_sha256: "test".into(),
                revision: "test".into(),
                versions: serde_json::json!({}),
            })?,
        )?;
        runner::replay(&replay, executable.clone())?;
    }
    Ok(())
}

#[test]
fn observations_and_process_isolation_preserve_failures() -> Result<()> {
    assert_ne!(number(0.0), number(-0.0));
    assert_ne!(number(f64::INFINITY), number(f64::NEG_INFINITY));
    assert_eq!(number(f64::NAN), number(-f64::NAN));
    let directory = tempfile::tempdir()?;
    let exited = command(
        Command::new("node").args(["-e", "process.exit(7)"]),
        directory.path(),
        Duration::from_secs(5),
    )?;
    assert_eq!(exited.exit, Some(7));
    assert!(!exited.success());
    let stalled = command(
        Command::new("node").args(["-e", "for(;;){}"]),
        directory.path(),
        Duration::from_millis(100),
    )?;
    assert!(stalled.timed_out && !stalled.success());
    Ok(())
}

#[test]
fn concrete_replay_retains_the_metamorphic_failure_after_emitter_changes() -> Result<()> {
    let case = Case::Program(Program {
        expression: Number::Integer(0),
        form: Form::Direct,
    });
    let variant = Case::Program(Program {
        expression: Number::Integer(0),
        form: Form::Loop,
    });
    let saved = SavedCase {
        contract: "ecma.number.arithmetic".into(),
        source: case.source(),
        case,
        metamorphic: vec![ConcreteCase {
            source: variant.source().replace("value:value", "value:1"),
            case: variant,
        }],
        settings: Settings::default(),
        source_sha256: "archived".into(),
        revision: "archived".into(),
        versions: serde_json::json!({}),
    };
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("metamorphic.json");
    fs::write(&path, serde_json::to_vec(&saved)?)?;
    let error = runner::replay(
        &path,
        PathBuf::from(env!("CARGO_BIN_EXE_perry-conformance")),
    )
    .unwrap_err();
    ensure!(
        error
            .to_string()
            .contains("metamorphic form changed Node observations"),
        "wrong replay failure: {error:#}"
    );
    Ok(())
}
