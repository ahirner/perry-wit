//! The catalog is a projection of executable contracts, never an input to execution.
use crate::{
    execution::{Evidence, check_equivalence},
    program::{Binary, Form, Math, Number, Program, Text},
    strategies,
    transitions::{Action, TimerModel},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use proptest_state_machine::ReferenceStateMachine;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "snake_case")]
pub enum Case {
    Program(Program),
    Fault {
        program: Program,
        fault: crate::execution::Fault,
    },
    Function {
        source: String,
        asynchronous: bool,
    },
    Command {
        source: String,
    },
    Reject {
        source: String,
        diagnostic: String,
    },
    Lifecycle(Vec<Action>),
}
impl Case {
    pub fn source(&self) -> String {
        match self {
            Self::Program(program) | Self::Fault { program, .. } => program.source(),
            Self::Lifecycle(actions) => crate::transitions::source(actions),
            Self::Function { source, .. }
            | Self::Command { source }
            | Self::Reject { source, .. } => source.clone(),
        }
    }
    pub fn asynchronous(&self) -> bool {
        match self {
            Self::Program(program) | Self::Fault { program, .. } => program.form.is_async(),
            Self::Function { asynchronous, .. } => *asynchronous,
            Self::Lifecycle(_) => true,
            _ => false,
        }
    }
}

pub struct Witness {
    pub partition: &'static str,
    pub case: Case,
}
pub struct Contract {
    pub id: &'static str,
    pub description: &'static str,
    pub specification: &'static str,
    pub domain: &'static str,
    pub witnesses: Vec<Witness>,
    pub strategy: BoxedStrategy<Case>,
    pub check: fn(&Case, &Evidence) -> Result<()>,
}
#[derive(Serialize)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub description: &'static str,
    pub specification: &'static str,
    pub domain: &'static str,
    pub partitions: Vec<&'static str>,
}
impl Contract {
    pub fn catalog(&self) -> CatalogEntry {
        CatalogEntry {
            id: self.id,
            description: self.description,
            specification: self.specification,
            domain: self.domain,
            partitions: self.witnesses.iter().map(|w| w.partition).collect(),
        }
    }
}

fn expression_contract(
    id: &'static str,
    description: &'static str,
    strategy: BoxedStrategy<Number>,
    examples: Vec<(&'static str, Number)>,
) -> Contract {
    Contract {
        id,
        description,
        specification: "https://tc39.es/ecma262/",
        domain: "Typed terminating numeric programs over the IEEE-754 boundary corpus; direct/local/branch/loop forms",
        witnesses: examples
            .into_iter()
            .map(|(partition, expression)| Witness {
                partition,
                case: Case::Program(Program {
                    expression,
                    form: Form::Direct,
                }),
            })
            .collect(),
        strategy: strategy
            .prop_map(|expression| {
                Case::Program(Program {
                    expression,
                    form: Form::Direct,
                })
            })
            .boxed(),
        check: check_equivalence,
    }
}

pub fn contracts(depth: u32) -> Result<Vec<Contract>> {
    let n = strategies::numbers(depth);
    let t = strategies::texts(depth);
    let input = || Box::new(Number::Input);
    let literal = || Box::new(Text::Literal("é\0漢".into()));
    let mut contracts = vec![
        expression_contract(
            "ecma.number.arithmetic",
            "IEEE arithmetic and evaluation order",
            n.clone(),
            vec![
                ("signed-zero", Number::NegativeZero),
                (
                    "division",
                    Number::Binary(Binary::Div, input(), Box::new(Number::Integer(0))),
                ),
                ("trace", Number::Mark(input())),
                ("call", Number::Call(input())),
            ],
        ),
        expression_contract(
            "ecma.math.rounding",
            "Numeric Math operations",
            (
                prop::sample::select(vec![
                    Math::Floor,
                    Math::Ceil,
                    Math::Trunc,
                    Math::Abs,
                    Math::Round,
                ]),
                n.clone(),
            )
                .prop_map(|(op, v)| Number::Math(op, Box::new(v)))
                .boxed(),
            [Math::Floor, Math::Ceil, Math::Trunc, Math::Abs, Math::Round]
                .into_iter()
                .zip(["floor", "ceil", "trunc", "abs", "round"])
                .map(|(op, id)| (id, Number::Math(op, input())))
                .collect(),
        ),
        expression_contract(
            "ecma.string.operations",
            "String search, casing, slicing and length",
            t.clone().prop_map(|v| Number::Length(Box::new(v))).boxed(),
            vec![
                ("unicode-and-nul", Number::Length(literal())),
                (
                    "search",
                    Number::IndexOf(literal(), Box::new(Text::Literal("漢".into())), input()),
                ),
                ("lower", Number::Length(Box::new(Text::Lower(literal())))),
                ("upper", Number::Length(Box::new(Text::Upper(literal())))),
                (
                    "slice",
                    Number::Length(Box::new(Text::Slice(literal(), -2, 3))),
                ),
                (
                    "char-at",
                    Number::Length(Box::new(Text::CharAt(literal(), input()))),
                ),
            ],
        ),
        expression_contract(
            "ecma.array.dense",
            "Dense numeric arrays preserve values and effects",
            (n.clone(), n.clone(), n.clone(), 0u8..3)
                .prop_map(|(a, b, c, i)| Number::Array(Box::new(a), Box::new(b), Box::new(c), i))
                .boxed(),
            vec![("index", Number::Array(input(), input(), input(), 1))],
        ),
        expression_contract(
            "ecma.object.records",
            "Record fields preserve values and effects",
            (n.clone(), n.clone())
                .prop_map(|(a, b)| Number::Field(Box::new(a), Box::new(b)))
                .boxed(),
            vec![(
                "field-order",
                Number::Field(input(), Box::new(Number::Mark(input()))),
            )],
        ),
        expression_contract(
            "ecma.date.epoch",
            "Date TimeClip and epoch reads",
            n.clone()
                .prop_map(|v| Number::DateTime(Box::new(v)))
                .boxed(),
            vec![("timeclip", Number::DateTime(input()))],
        ),
        expression_contract(
            "ecma.uint8array.conversion",
            "Byte conversion and indexed access",
            (n.clone(), n.clone(), n.clone(), 0u8..3)
                .prop_map(|(a, b, c, i)| Number::ByteGet(Box::new(a), Box::new(b), Box::new(c), i))
                .boxed(),
            vec![("wrapping", Number::ByteGet(input(), input(), input(), 0))],
        ),
        expression_contract(
            "web.textencoder.subviews",
            "UTF-8 encoding and byte subviews",
            (t.clone(), n.clone(), n.clone())
                .prop_map(|(a, b, c)| Number::EncodedLength(Box::new(a), Box::new(b), Box::new(c)))
                .boxed(),
            vec![(
                "subview-bounds",
                Number::EncodedLength(literal(), input(), Box::new(Number::Integer(8))),
            )],
        ),
        expression_contract(
            "ecma.json.roundtrip",
            "JSON text survives direct and stored decoding",
            (t, any::<bool>())
                .prop_map(|(v, stored)| {
                    if stored {
                        Number::JsonStoredLength(Box::new(v))
                    } else {
                        Number::JsonLength(Box::new(v))
                    }
                })
                .boxed(),
            vec![
                ("direct", Number::JsonLength(literal())),
                ("stored", Number::JsonStoredLength(literal())),
            ],
        ),
    ];
    for (id, description, form) in [
        ("node.timers.promises.value", "Timer result", Form::Timer),
        (
            "node.timers.promises.observers",
            "Repeated timer observation",
            Form::StoredTimer,
        ),
        ("node.fs.readFile.utf8", "UTF-8 file read", Form::FileRead),
        (
            "node.fs.writeFile.text",
            "Text file round trip",
            Form::FileRoundTrip,
        ),
        (
            "node.fs.metadata",
            "File stat and directory listing",
            Form::FileMetadata,
        ),
        (
            "node.fs.readFile.bytes",
            "Binary file read and UTF-8 decoding",
            Form::FileBytes,
        ),
        (
            "node.fs.writeFile.subviews",
            "Byte view and empty file round trip",
            Form::FileByteRoundTrip,
        ),
        (
            "node.fs.readFile.recovery",
            "Repeated rejection followed by successful read",
            Form::FileRejectedRead,
        ),
    ] {
        contracts.push(Contract {id,description,specification:"https://nodejs.org/api/",domain:"Retained async operation interleaved with typed numeric evaluation; instance reused across boundary inputs",
            witnesses:vec![Witness{partition:"retained-operation",case:Case::Program(Program{expression:Number::Mark(input()),form})}],
            strategy:n.clone().prop_map(move|expression|Case::Program(Program{expression,form})).boxed(),check:check_equivalence});
    }
    let regressions = [
        (
            "branch-true",
            include_str!("../cases/regression-1.json"),
            "ecma.array.dense",
        ),
        (
            "branch-false",
            include_str!("../cases/regression-2.json"),
            "ecma.array.dense",
        ),
        (
            "first-index",
            include_str!("../cases/regression-3.json"),
            "ecma.array.dense",
        ),
        (
            "array-effects",
            include_str!("../cases/regression-4.json"),
            "ecma.array.dense",
        ),
        (
            "byte-input",
            include_str!("../cases/regression-5.json"),
            "ecma.uint8array.conversion",
        ),
        (
            "byte-effects",
            include_str!("../cases/regression-6.json"),
            "ecma.uint8array.conversion",
        ),
        (
            "nested-codecs",
            include_str!("../cases/regression-7.json"),
            "ecma.date.epoch",
        ),
        (
            "stored-unicode",
            include_str!("../cases/regression-8.json"),
            "ecma.json.roundtrip",
        ),
        (
            "direct-unicode",
            include_str!("../cases/regression-9.json"),
            "ecma.json.roundtrip",
        ),
    ];
    for (partition, json, id) in regressions {
        contracts
            .iter_mut()
            .find(|c| c.id == id)
            .unwrap()
            .witnesses
            .push(Witness {
                partition,
                case: Case::Program(serde_json::from_str(json)?),
            });
    }
    for (id, source) in [
        ("ecma.json.command", include_str!("../cases/json.ts")),
        (
            "web.console.routing",
            include_str!("../cases/console_streams.ts"),
        ),
        ("ecma.promise.all", include_str!("../cases/promise_all.ts")),
        (
            "web.performance.monotonic",
            include_str!("../cases/clocks.ts"),
        ),
        ("ecma.date.clock", include_str!("../cases/date.ts")),
        ("web.crypto.random", include_str!("../cases/random.ts")),
        ("node.process.environment", include_str!("../cases/env.ts")),
    ] {
        let case = Case::Command {
            source: source.into(),
        };
        contracts.push(Contract {
            id,
            description: id,
            specification: "https://nodejs.org/api/",
            domain: "Standard CLI source; exact output bytes and successful exit",
            witnesses: vec![Witness {
                partition: "standard-source",
                case: case.clone(),
            }],
            strategy: Just(case).boxed(),
            check: check_equivalence,
        });
    }
    contracts.push(Contract {
        id: "node.timers.promises.lifecycle",
        description: "Start, settle, cancel, observe and reuse retained timer tasks",
        specification: "https://nodejs.org/api/timers.html",
        domain: "Explicit sequential lifecycle traces, including overlapping native work",
        witnesses: vec![
            Witness {
                partition: "settlement-and-reobserve",
                case: Case::Lifecycle(vec![
                    Action::Start,
                    Action::Release,
                    Action::Observe,
                    Action::Observe,
                ]),
            },
            Witness {
                partition: "cancel-and-reuse",
                case: Case::Lifecycle(vec![
                    Action::Start,
                    Action::Cancel,
                    Action::Observe,
                    Action::Reuse,
                    Action::Start,
                    Action::Release,
                    Action::Observe,
                ]),
            },
        ],
        strategy: TimerModel::sequential_strategy(1..=12)
            .prop_map(|(_, actions, _)| Case::Lifecycle(actions))
            .boxed(),
        check: crate::transitions::check,
    });
    for (id, fault) in [
        (
            "wasi.execution.detect_value_fault",
            crate::execution::Fault::Value,
        ),
        (
            "wasi.execution.detect_trace_fault",
            crate::execution::Fault::Trace,
        ),
        (
            "wasi.execution.detect_resource_fault",
            crate::execution::Fault::Resource,
        ),
    ] {
        let case = move |expression| Case::Fault {
            program: Program {
                expression: Number::Mark(Box::new(expression)),
                form: Form::Direct,
            },
            fault,
        };
        contracts.push(Contract{id,description:"Injected faults must be distinguished from the unchanged program",specification:"https://component-model.bytecodealliance.org/",domain:"Value corruption, omitted effects, and retained host resources; only the component adapter is faulted",
            witnesses:vec![Witness{partition:"injected-fault",case:case(Number::Input)}],strategy:n.clone().prop_map(case).boxed(),check:crate::execution::check_fault});
    }
    let mut ids = std::collections::HashSet::new();
    for contract in &contracts {
        ensure!(
            ids.insert(contract.id),
            "duplicate contract {}",
            contract.id
        );
        ensure!(
            !contract.witnesses.is_empty(),
            "contract {} has no directed witnesses",
            contract.id
        );
        let mut partitions = std::collections::HashSet::new();
        for witness in &contract.witnesses {
            ensure!(
                partitions.insert(witness.partition),
                "duplicate partition in {}",
                contract.id
            );
        }
    }
    Ok(contracts)
}
