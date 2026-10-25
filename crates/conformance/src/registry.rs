//! Read-only catalog projected from executable conformance contract definitions.
pub use crate::catalog::CatalogEntry;
pub(crate) use crate::catalog::ExpectedOutcome;
use crate::{
    execution::{Evidence, check_equivalence},
    program::{Binary, Form, INPUTS, Math, Number, Program, Text},
    strategies,
    transitions::{Action, TimerModel},
};
use anyhow::{Context, Result, ensure};
use proptest::prelude::*;
use proptest_state_machine::ReferenceStateMachine;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Success,
    Failure,
}
impl ExitStatus {
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "snake_case")]
pub enum Case {
    Program(Program),
    Aggregate(crate::aggregates::AggregateCase),
    Collection(crate::collections::CollectionCase),
    Stream(crate::streams::StreamCase),
    Fetch(crate::fetch::FetchCase),
    Handler(crate::handler::HandlerCase),
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
        exit: ExitStatus,
    },
    Reject {
        source: String,
        diagnostic: String,
    },
    Lifecycle(Vec<Action>),
    /// Calls `run(base)`, `run(base)`, `run(base * factor)` on one instance and bounds fuel growth.
    Scaling {
        source: String,
        base: u32,
        factor: u32,
        max_growth: u32,
    },
}
impl Case {
    pub(crate) fn expected_outcome(&self) -> ExpectedOutcome {
        match self {
            Self::Reject { .. } => ExpectedOutcome::CompilerRejection,
            Self::Fault { .. } => ExpectedOutcome::InjectedFaultDetection,
            Self::Scaling { .. } => ExpectedOutcome::BoundedGrowth,
            _ => ExpectedOutcome::EquivalentExecution,
        }
    }
    /// The case followed by its metamorphic forms, which must observe identically.
    pub(crate) fn variants(&self) -> Vec<Cow<'_, Self>> {
        match self {
            Self::Program(program) if program.form == Form::Direct => Form::ALL
                .into_iter()
                .map(|form| {
                    if form == Form::Direct {
                        Cow::Borrowed(self)
                    } else {
                        Cow::Owned(Self::Program(Program {
                            expression: program.expression.clone(),
                            form,
                        }))
                    }
                })
                .collect(),
            Self::Program(program) if program.form == Form::StoredTimer => Form::TIMER_OBSERVERS
                .into_iter()
                .map(|form| {
                    if form == Form::StoredTimer {
                        Cow::Borrowed(self)
                    } else {
                        Cow::Owned(Self::Program(Program {
                            expression: program.expression.clone(),
                            form,
                        }))
                    }
                })
                .collect(),
            Self::Aggregate(case) if !case.permuted => vec![
                Cow::Borrowed(self),
                Cow::Owned(Self::Aggregate(crate::aggregates::AggregateCase {
                    permuted: true,
                    ..case.clone()
                })),
            ],
            Self::Collection(case) if !case.permuted => vec![
                Cow::Borrowed(self),
                Cow::Owned(Self::Collection(crate::collections::CollectionCase {
                    permuted: true,
                    ..case.clone()
                })),
            ],
            _ => vec![Cow::Borrowed(self)],
        }
    }
    /// Arbitrary function sources receive the full boundary corpus. Structured cases can
    /// prove the input irrelevant and use three calls to exercise instance reuse instead.
    pub(crate) fn inputs(&self) -> &'static [f64] {
        let uses_all = match self {
            Self::Lifecycle(_)
            | Self::Stream(_)
            | Self::Fetch(_)
            | Self::Handler(_)
            | Self::Scaling { .. } => false,
            Self::Program(program) | Self::Fault { program, .. } => program.expression.uses_input(),
            _ => true,
        };
        if uses_all { &INPUTS } else { &INPUTS[..3] }
    }
    pub fn source(&self) -> String {
        match self {
            Self::Aggregate(case) => case.source(),
            Self::Collection(case) => case.source(),
            Self::Stream(case) => case.source(),
            Self::Fetch(case) => case.source(),
            Self::Handler(case) => case.source(),
            Self::Program(program) | Self::Fault { program, .. } => program.source(),
            Self::Lifecycle(actions) => crate::transitions::source(actions),
            Self::Function { source, .. }
            | Self::Command { source, .. }
            | Self::Reject { source, .. }
            | Self::Scaling { source, .. } => source.clone(),
        }
    }
    pub(crate) fn asynchronous(&self) -> bool {
        match self {
            Self::Program(program) | Self::Fault { program, .. } => program.form.is_async(),
            Self::Function { asynchronous, .. } => *asynchronous,
            Self::Lifecycle(_) | Self::Stream(_) | Self::Fetch(_) | Self::Handler(_) => true,
            _ => false,
        }
    }
}

pub struct Witness {
    pub(crate) partition: &'static str,
    pub case: Case,
}
pub struct Contract {
    pub id: &'static str,
    pub witnesses: Vec<Witness>,
    pub(crate) strategy: BoxedStrategy<Case>,
    pub(crate) check: fn(&Case, &Evidence) -> Result<()>,
}
impl Contract {
    pub(crate) fn new(
        id: &'static str,
        witnesses: Vec<Witness>,
        strategy: BoxedStrategy<Case>,
        check: fn(&Case, &Evidence) -> Result<()>,
    ) -> Self {
        Self {
            id,
            witnesses,
            strategy,
            check,
        }
    }

    /// The contract's documented metadata from the static catalog.
    pub fn catalog(&self) -> Result<&'static CatalogEntry> {
        crate::catalog::entry(self.id)
            .with_context(|| format!("contract {} has no catalog entry", self.id))
    }

    /// Ensure the catalog entry describes exactly these witnesses: their partitions in order
    /// and their distinct expected outcomes in first-appearance order.
    fn check_catalog(&self, entry: &CatalogEntry) -> Result<()> {
        ensure!(
            crate::catalog::namespace(self.id).is_some(),
            "contract {} has no known namespace",
            self.id
        );
        ensure!(
            self.witnesses
                .iter()
                .map(|witness| witness.partition)
                .eq(entry.partitions.iter().copied()),
            "catalog partitions of {} differ from its witnesses",
            self.id
        );
        let mut outcomes = Vec::new();
        for witness in &self.witnesses {
            let outcome = witness.case.expected_outcome();
            if !outcomes.contains(&outcome) {
                outcomes.push(outcome);
            }
        }
        ensure!(
            outcomes == entry.expected_outcomes,
            "catalog outcomes of {} differ from its witnesses",
            self.id
        );
        Ok(())
    }
}

fn expression_contract(
    id: &'static str,
    strategy: BoxedStrategy<Number>,
    examples: Vec<(&'static str, Number)>,
) -> Contract {
    Contract::new(
        id,
        examples
            .into_iter()
            .map(|(partition, expression)| Witness {
                partition,
                case: Case::Program(Program {
                    expression,
                    form: Form::Direct,
                }),
            })
            .collect(),
        strategy
            .prop_map(|expression| {
                Case::Program(Program {
                    expression,
                    form: Form::Direct,
                })
            })
            .boxed(),
        check_equivalence,
    )
}

pub fn contracts(depth: u32) -> Result<Vec<Contract>> {
    let n = strategies::numbers(depth);
    let t = strategies::texts(depth);
    let input = || Box::new(Number::Input);
    let literal = || Box::new(Text::Literal("é\0漢".into()));
    let mut bitwise = expression_contract(
        "ecma.number.bitwise",
        strategies::bitwise(n.clone()),
        vec![
            (
                "and",
                Number::Binary(
                    Binary::BitAnd,
                    input(),
                    Box::new(Number::Integer(0x81234567)),
                ),
            ),
            (
                "or",
                Number::Binary(Binary::BitOr, input(), Box::new(Number::Integer(0))),
            ),
            (
                "xor",
                Number::Binary(Binary::BitXor, input(), Box::new(Number::Integer(-1))),
            ),
            ("not", Number::BitNot(input())),
            (
                "left",
                Number::Binary(Binary::Shl, input(), Box::new(Number::Integer(31))),
            ),
            (
                "right",
                Number::Binary(Binary::Shr, input(), Box::new(Number::Integer(1))),
            ),
            (
                "unsigned-right",
                Number::Binary(Binary::UShr, input(), Box::new(Number::Integer(0))),
            ),
            (
                "imul",
                Number::Imul(input(), Box::new(Number::Integer(0x01000193))),
            ),
            (
                "trace",
                Number::Imul(
                    Box::new(Number::Mark(Box::new(Number::Binary(
                        Binary::BitXor,
                        input(),
                        Box::new(Number::Integer(7)),
                    )))),
                    Box::new(Number::Mark(Box::new(Number::Integer(0xffffffff)))),
                ),
            ),
        ],
    );
    bitwise.witnesses.push(Witness {
        partition: "counts-and-compound-assignments",
        case: Case::Function {
            asynchronous: false,
            source: r#"
export function run(x: number): {value: number, trace: number[]} {
    const trace: number[] = [];
    const counts = [-1, 0, 1, 31, 32, 33, 63, 64, 4294967295, 4294967296, x];
    for (const count of counts) {
        trace.push(x << count, x >> count, x >>> count);
        trace.push(0x81234567 << count, 0x81234567 >> count, 0x81234567 >>> count);
        trace.push(Math.imul(x, count), Math.imul(count, x));
    }
    const boundaries = [
        -4294967297.75, -4294967296.75, -4294967295.75,
        -2147483649.75, -2147483648.75, -2147483647.75,
        2147483647.75, 2147483648.75, 2147483649.75,
        4294967295.75, 4294967296.75, 4294967297.75,
    ];
    for (const boundary of boundaries) {
        trace.push(boundary | 0, boundary >>> 0, ~boundary);
        trace.push(boundary ^ x, Math.imul(boundary, x), Math.imul(x, boundary));
    }
    let value = x;
    value |= 0; trace.push(value);
    value &= 0xfedcba98; trace.push(value);
    value ^= 0x81234567; trace.push(value);
    value <<= 33; trace.push(value);
    value >>= 31; trace.push(value);
    value >>>= 0; trace.push(value);
    let checksum = 0x811c9dc5 ^ x;
    for (let i = 0; i < 100; i++) {
        checksum = Math.imul(checksum ^ i, 0x01000193);
        trace.push(checksum, checksum >>> 0);
    }
    trace.push(checksum, checksum >>> 0);
    return {value, trace};
}
"#
            .into(),
        },
    });
    let mut contracts = vec![
        expression_contract(
            "ecma.number.arithmetic",
            n.clone(),
            vec![
                ("signed-zero", Number::NegativeZero),
                (
                    "division",
                    Number::Binary(Binary::Div, input(), Box::new(Number::Integer(0))),
                ),
                ("trace", Number::Mark(input())),
                ("call", Number::Call(input())),
                ("record-field-update", Number::ClosedLoop(input(), 4)),
                (
                    "byte-alias",
                    Number::ByteAlias(input(), Box::new(Number::Integer(7))),
                ),
                ("checked-json", Number::TypedJson(input())),
                ("handled-throw", Number::Handled(input())),
            ],
        ),
        bitwise,
        expression_contract(
            "ecma.math.rounding",
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
                .chain(std::iter::once((
                    "abs-widened-loop",
                    Number::Array(
                        Box::new(Number::Mark(Box::new(Number::Integer(0)))),
                        Box::new(Number::Mark(Box::new(Number::Integer(3)))),
                        Box::new(Number::Math(
                            Math::Abs,
                            Box::new(Number::ClosedLoop(
                                Box::new(Number::Mark(Box::new(Number::Mark(Box::new(
                                    Number::Integer(-472),
                                ))))),
                                1,
                            )),
                        )),
                        2,
                    ),
                )))
                .collect(),
        ),
        expression_contract(
            "ecma.string.operations",
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
            (n.clone(), n.clone(), n.clone(), 0u8..3)
                .prop_map(|(a, b, c, i)| Number::Array(Box::new(a), Box::new(b), Box::new(c), i))
                .boxed(),
            vec![("index", Number::Array(input(), input(), input(), 1))],
        ),
        expression_contract(
            "ecma.object.records",
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
            n.clone()
                .prop_map(|v| Number::DateTime(Box::new(v)))
                .boxed(),
            vec![("timeclip", Number::DateTime(input()))],
        ),
        expression_contract(
            "ecma.uint8array.conversion",
            (n.clone(), n.clone(), n.clone(), 0u8..3)
                .prop_map(|(a, b, c, i)| Number::ByteGet(Box::new(a), Box::new(b), Box::new(c), i))
                .boxed(),
            vec![("wrapping", Number::ByteGet(input(), input(), input(), 0))],
        ),
        expression_contract(
            "web.textencoder.subviews",
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
    for (id, form) in [
        ("node.timers.promises.value", Form::Timer),
        ("node.timers.promises.observers", Form::StoredTimer),
    ] {
        contracts.push(Contract::new(
            id,
            vec![Witness {
                partition: "retained-operation",
                case: Case::Program(Program {
                    expression: Number::Mark(input()),
                    form,
                }),
            }],
            n.clone()
                .prop_map(move |expression| Case::Program(Program { expression, form }))
                .boxed(),
            check_equivalence,
        ));
    }
    contracts.extend(crate::capabilities::fs_contracts());
    let regressions = [
        (
            "branch-true",
            include_str!("../cases/branch-true-array.json"),
            "ecma.array.dense",
        ),
        (
            "branch-false",
            include_str!("../cases/branch-false-array.json"),
            "ecma.array.dense",
        ),
        (
            "first-index",
            include_str!("../cases/first-array-index.json"),
            "ecma.array.dense",
        ),
        (
            "array-effects",
            include_str!("../cases/array-effects.json"),
            "ecma.array.dense",
        ),
        (
            "byte-input",
            include_str!("../cases/byte-input.json"),
            "ecma.uint8array.conversion",
        ),
        (
            "byte-effects",
            include_str!("../cases/byte-effects.json"),
            "ecma.uint8array.conversion",
        ),
        (
            "nested-codecs",
            include_str!("../cases/nested-codecs.json"),
            "ecma.date.epoch",
        ),
        (
            "stored-unicode",
            include_str!("../cases/stored-json-unicode.json"),
            "ecma.json.roundtrip",
        ),
        (
            "direct-unicode",
            include_str!("../cases/direct-json-unicode.json"),
            "ecma.json.roundtrip",
        ),
    ];
    for (partition, json, id) in regressions {
        contracts
            .iter_mut()
            .find(|c| c.id == id)
            .with_context(|| format!("regression {partition} names unknown contract {id}"))?
            .witnesses
            .push(Witness {
                partition,
                case: Case::Program(serde_json::from_str(json)?),
            });
    }
    for (id, source) in [
        ("ecma.json.command", include_str!("../cases/json.ts")),
        ("ecma.command.arrays", include_str!("../cases/arrays.ts")),
        ("ecma.command.text", include_str!("../cases/text.ts")),
        ("ecma.command.records", include_str!("../cases/records.ts")),
        ("ecma.command.numbers", include_str!("../cases/numbers.ts")),
        ("ecma.command.web", include_str!("../cases/web.ts")),
        (
            "ecma.command.temporal",
            include_str!("../cases/temporal.ts"),
        ),
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
            exit: ExitStatus::Success,
        };
        contracts.push(Contract::new(
            id,
            vec![Witness {
                partition: "standard-source",
                case: case.clone(),
            }],
            Just(case).boxed(),
            check_equivalence,
        ));
    }
    contracts.push(Contract::new(
        "node.timers.promises.lifecycle",
        vec![
            Witness {
                partition: "settlement-and-reobserve",
                case: Case::Lifecycle(crate::transitions::complete_trace(vec![
                    Action::Start,
                    Action::HostRelease,
                    Action::Complete,
                    Action::Observe,
                    Action::Observe,
                ])),
            },
            Witness {
                partition: "cancel-and-reuse",
                case: Case::Lifecycle(crate::transitions::complete_trace(vec![
                    Action::Start,
                    Action::Cancel,
                    Action::Complete,
                    Action::Observe,
                    Action::Reuse,
                    Action::Start,
                    Action::HostRelease,
                    Action::Complete,
                    Action::Observe,
                ])),
            },
        ],
        TimerModel::sequential_strategy(1..=12)
            .prop_map(|(_, actions, _)| {
                Case::Lifecycle(crate::transitions::complete_trace(actions))
            })
            .boxed(),
        crate::transitions::check,
    ));
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
        contracts.push(Contract::new(
            id,
            vec![Witness {
                partition: "injected-fault",
                case: case(Number::Input),
            }],
            n.clone().prop_map(case).boxed(),
            crate::execution::check_fault,
        ));
    }
    contracts.push(crate::streams::contract());
    contracts.push(crate::streams::byob_contract());
    contracts.push(crate::fetch::contract());
    contracts.push(crate::fetch::byob_contract());
    contracts.extend(crate::capabilities::contracts());
    contracts.push(crate::handler::contract());
    contracts.extend(crate::scaling::contracts());
    contracts.push(crate::aggregates::contract());
    contracts.push(crate::collections::contract());
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
    for id in crate::PASSING
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        ensure!(
            ids.contains(id.trim()),
            "passing.txt names unknown contract {id}"
        );
    }
    for (id, reason) in crate::deferrals() {
        ensure!(ids.contains(id), "deferred.txt names unknown contract {id}");
        ensure!(!reason.is_empty(), "deferred contract {id} needs a reason");
        ensure!(!crate::passing(id), "{id} is both passing and deferred");
    }
    let entries = crate::catalog::ENTRIES;
    ensure!(
        contracts.len() == entries.len(),
        "the catalog lists {} contracts, the registry defines {}",
        entries.len(),
        contracts.len()
    );
    for (contract, entry) in contracts.iter().zip(entries) {
        ensure!(
            contract.id == entry.id,
            "catalog order differs at {}: expected {}",
            contract.id,
            entry.id
        );
        contract.check_catalog(entry)?;
    }
    Ok(contracts)
}

/// The handbook's catalog page, after checking the catalog against the registry.
pub fn markdown() -> Result<String> {
    contracts(0)?;
    crate::catalog::render_markdown(crate::catalog::ENTRIES).map_err(anyhow::Error::msg)
}
