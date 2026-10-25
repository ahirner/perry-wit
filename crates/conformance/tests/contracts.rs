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
use std::{fs, process::Command, time::Duration};

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
    let executor = Executor::new(Limits::default());
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
        runner::replay(&replay)?;
    }
    Ok(())
}

#[test]
fn observations_and_process_isolation_preserve_failures() -> Result<()> {
    assert_ne!(number(0.0), number(-0.0));
    assert_ne!(number(f64::INFINITY), number(f64::NEG_INFINITY));
    assert_eq!(number(f64::NAN), number(-f64::NAN));
    let directory = tempfile::tempdir()?;
    let executor = Executor::new(Limits::default());
    let mut baseline = None;
    for expression in ["x", "identity(0,x)", "identity(0, x)"] {
        let case = Case::Function {
            asynchronous: false,
            source: format!(
                "function identity(unused:number,value:number):number {{ return value; }}
                 export function run(x:number):{{value:number,trace:number[]}} {{
                     return {{value:{expression},trace:[]}};
                 }}"
            ),
        };
        let artifacts = tempfile::tempdir_in(directory.path())?;
        let evidence = executor.check(&case, artifacts.path())?;
        let evidence = serde_json::to_value(&evidence[0])?;
        let observations = &evidence["oracle"]["detail"]["observations"];
        for boundary in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 4294967295.0] {
            ensure!(
                observations
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|o| o["value"] == number(boundary)),
                "function case missed boundary {boundary}"
            );
        }
        if let Some(baseline) = &baseline {
            assert_eq!(
                observations, baseline,
                "source formatting changed the input domain"
            );
        } else {
            baseline = Some(observations.clone());
        }
    }
    let exited = command(
        Command::new("node").args(["-e", "process.exit(7)"]),
        directory.path(),
        Duration::from_secs(Limits::default().timeout_seconds),
    )?;
    assert!(!exited.timed_out && !exited.success());
    assert_eq!(exited.exit, Some(7));
    let stalled = command(
        Command::new("node").args(["-e", "for(;;){}"]),
        directory.path(),
        Duration::from_millis(100),
    )?;
    assert!(stalled.timed_out && !stalled.success());
    let case = Case::Program(Program {
        expression: Number::Integer(0),
        form: Form::Timer,
    });
    let mut settings = Settings::default();
    settings.limits.timeout_seconds = 1;
    let slow_path = directory.path().join("slow-timer.json");
    fs::write(
        &slow_path,
        serde_json::to_vec(&SavedCase {
            contract: "ecma.number.arithmetic".into(),
            source: case.source().replace("setTimeout(0,", "setTimeout(3000,"),
            case: case.clone(),
            metamorphic: vec![],
            settings: settings.clone(),
            source_sha256: "test".into(),
            revision: "test".into(),
            versions: serde_json::json!({}),
        })?,
    )?;
    let start = std::time::Instant::now();
    let error = runner::replay(&slow_path).unwrap_err();
    ensure!(
        error
            .to_string()
            .contains("oracle: Timeout, component: Timeout"),
        "expected oracle and component timeout: {error:#}"
    );
    assert!(start.elapsed() < Duration::from_millis(2500));
    let ok_path = directory.path().join("recovered.json");
    fs::write(
        &ok_path,
        serde_json::to_vec(&SavedCase {
            contract: "ecma.number.arithmetic".into(),
            source: case.source(),
            case,
            metamorphic: vec![],
            settings,
            source_sha256: "test".into(),
            revision: "test".into(),
            versions: serde_json::json!({}),
        })?,
    )?;
    runner::replay(&ok_path)?;
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
    let error = runner::replay(&path).unwrap_err();
    ensure!(
        error
            .to_string()
            .contains("metamorphic form changed Node observations"),
        "wrong replay failure: {error:#}"
    );
    Ok(())
}

#[test]
fn orthogonal_aggregate_and_collection_generators_and_metamorphic_invariants() -> Result<()> {
    for (select, seed) in [
        ("ecma.aggregate.specialization", 42),
        ("ecma.aggregate.specialization", 43),
        ("ecma.collection.iterators", 42),
    ] {
        runner::campaign(Settings {
            select: select.into(),
            cases: 6,
            depth: 3,
            seed,
            shrink_limit: 32,
            limits: Limits::default(),
        })?;
    }
    Ok(())
}
