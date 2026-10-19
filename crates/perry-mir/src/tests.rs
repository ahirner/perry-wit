mod effects;
mod facts;
mod optimization;

use crate::{
    attrs::{BinaryKind, CompareKind},
    calls::{AwaitOp, set_async, set_atomic_construction, set_exported},
    collections::{ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp},
    control::ReturnOp,
    effects::Failure,
    facts::{
        ProgramFacts, constrain_signature, specialize, specialized_implementation,
        verify_lossless_module, with_fact_limits,
    },
    heap::FieldGetOp,
    intrinsics::{Combinator, IntrinsicOp, PromiseCombinatorOp},
    liveness::LiveValues,
    pipeline::{FnPass, canonicalize},
    scalar::{BinaryOp, CompareOp},
};
use pliron::{
    analyses::liveness::{Liveness, LivenessTq},
    attribute::AttrObj,
    builtin::{
        attr_interfaces::TypedAttrInterface,
        attributes::{FPDoubleAttr, IntegerAttr, TypeAttr},
        op_interfaces::{OneRegionInterface, OneResultInterface, SymbolOpInterface},
        ops::{ConstantOp, FuncOp},
        type_interfaces::FunctionTypeInterface,
        types::{FP64Type, FunctionType, IntegerType, Signedness},
    },
    common_traits::Verify,
    context::{Context, Ptr},
    graph::{
        dominance::DomInfo,
        walkers::{IRNode, WALKCONFIG_PREORDER_FORWARD, uninterruptible::immutable::walk_op},
    },
    irbuild::{IRStatus, inserter::OpInsertionPoint, match_rewrite::PassWrapper},
    irfmt::parsers::spaced,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::{Operation, verify_operation},
    opts::{constants::ConstFoldInterface, dce::dce, mem2reg::mem2reg},
    parsable::parse_from_str,
    pass::{Analysis, AnalysisManager, Pass, PassResult, Passes},
    printable::Printable,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};
use std::{collections::BTreeSet, fmt::Write};

/// The temporary-record workload of `wasi.execution.scaling.temporary_records`, as the HIR→MIR
/// builder (P2.2) is expected to emit it: `make` inlined, tier not yet classified.
const TEMPORARY_RECORDS: &str = r#"
builtin.module @scaling {
  ^module():
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(n: builtin.fp64):
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        perry.br ^header(zero, zero)
      ^header(i: builtin.fp64, sum: builtin.fp64):
        more = perry.compare Lt i, n : builtin.integer i1;
        perry.cond_br if more ^body() else ^exit()
      ^body():
        two = builtin.constant <builtin.double 2.0> : builtin.fp64;
        doubled = perry.binary Mul i, two : builtin.fp64;
        point = perry.alloc Unassigned (i, doubled) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>;
        x = perry.field_get point."x" : builtin.fp64;
        y = perry.field_get point."y" : builtin.fp64;
        partial = perry.binary Add sum, x : builtin.fp64;
        total = perry.binary Add partial, y : builtin.fp64;
        modulus = builtin.constant <builtin.double 1000003.0> : builtin.fp64;
        next_sum = perry.binary Rem total, modulus : builtin.fp64;
        one = builtin.constant <builtin.double 1.0> : builtin.fp64;
        next_i = perry.binary Add i, one : builtin.fp64;
        perry.br ^header(next_i, next_sum)
      ^exit():
        perry.return sum
    }
}
"#;

fn parse(ctx: &mut Context, input: &str) -> Ptr<Operation> {
    parse_from_str(spaced(Operation::top_level_parser()), ctx, input)
        .unwrap_or_else(|error| panic!("{}", error.disp(ctx)))
}

fn print(ctx: &Context, op: Ptr<Operation>) -> String {
    Operation::get_op_dyn(op, ctx).disp(ctx).to_string()
}

/// The function called `name` in `module`.
fn function(ctx: &Context, module: Ptr<Operation>, name: &str) -> FuncOp {
    crate::aggregates::functions(module, ctx)
        .into_iter()
        .find(|f| f.get_symbol_name(ctx).as_ref() == name)
        .unwrap_or_else(|| panic!("no function {name}"))
}

/// The first operation of kind `O` in `function`.
fn first<O: Op>(ctx: &Context, function: &FuncOp) -> Ptr<Operation> {
    crate::aggregates::operations(ctx, function)
        .into_iter()
        .find(|&op| Operation::get_op::<O>(op, ctx).is_some())
        .unwrap_or_else(|| panic!("no {}", O::get_opid_static()))
}

/// Freshly computed program facts, bypassing any analysis cache.
fn facts(ctx: &Context, module: Ptr<Operation>) -> Result<ProgramFacts> {
    ProgramFacts::compute(module, ctx, &mut AnalysisManager::default())
}

/// The value SCCP folds `op(lhs, rhs)` to.
fn fold<O: ConstFoldInterface>(
    ctx: &mut Context,
    op: impl FnOnce(&mut Context, Value, Value) -> O,
    lhs: impl TypedAttrInterface + Clone,
    rhs: impl TypedAttrInterface + Clone,
) -> AttrObj {
    let a = ConstantOp::new(ctx, Box::new(lhs.clone())).get_result(ctx);
    let b = ConstantOp::new(ctx, Box::new(rhs.clone())).get_result(ctx);
    op(ctx, a, b)
        .check_fold(ctx, &[Some(Box::new(lhs)), Some(Box::new(rhs))])
        .into_iter()
        .next()
        .flatten()
        .expect("constant operands fold")
}

/// Normalize printer artifacts that depend on parse order: location references (` !N`, which
/// point into an outline table `disp` omits) and uniquing suffixes on SSA and block names.
fn normalized(text: &str) -> String {
    let locations = regex::Regex::new(r" !\d+").unwrap();
    let names = regex::Regex::new(r"(_block\d+v\d+|_v\d+)+\b").unwrap();
    let text = locations.replace_all(text, "");
    let text = names.replace_all(&text, "");
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn round_trip(ctx: &Context, module: Ptr<Operation>) -> Result<String> {
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    let reparsed_ctx = &mut Context::new();
    let reparsed = parse(reparsed_ctx, &normalized(&printed));
    verify_operation(reparsed, reparsed_ctx)?;
    assert_eq!(
        normalized(&printed),
        normalized(&print(reparsed_ctx, reparsed))
    );
    Ok(printed)
}

#[test]
fn temporary_record_loop_round_trips_and_verifies() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(ctx, TEMPORARY_RECORDS);
    let alloc = Operation::get_op::<crate::AllocOp>(
        first::<crate::AllocOp>(ctx, &function(ctx, module, "run")),
        ctx,
    )
    .unwrap();
    alloc.set_attr_perry_field_order(ctx, crate::FieldOrder(vec![1, 0]));
    let printed = round_trip(ctx, module)?;
    for expected in [
        "perry.alloc Unassigned (",
        "perry.record <Point { x: builtin.fp64",
        "perry.field_get",
        "perry.cond_br if",
        "perry.binary Rem",
    ] {
        assert!(
            printed.contains(expected),
            "missing {expected} in\n{printed}"
        );
    }
    let invalid = TEMPORARY_RECORDS.replace(
        "more = perry.compare Lt i, n : builtin.integer i1;",
        "more = builtin.constant <builtin.integer <1: i32>> : builtin.integer i32;",
    );
    assert!(
        verify_text(&invalid)
            .unwrap_err()
            .contains("condition must be a signless i1")
    );
    Ok(())
}

fn verify_text(input: &str) -> std::result::Result<(), String> {
    let ctx = &mut Context::new();
    let module = parse_from_str(spaced(Operation::top_level_parser()), ctx, input)
        .map_err(|error| error.disp(ctx).to_string())?;
    verify_operation(module, ctx).map_err(|error| error.disp(ctx).to_string())
}

fn single_block(body: &str) -> String {
    format!(
        r#"
builtin.module @m {{
  ^module():
    builtin.func @f: builtin.function <(builtin.fp64) -> ()> {{
      ^entry(v: builtin.fp64):
        {body}
        perry.return
    }}
}}
"#
    )
}

#[test]
fn verifier_rejects_ill_typed_heap_ops() {
    const POINT: &str = "perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>";
    let flag = "builtin.integer i1";
    let cases = [
        (
            format!("b = perry.undef : {flag}; p = perry.alloc Unassigned (b) : {POINT};"),
            "has 2 fields but perry.alloc supplies 1",
        ),
        (
            format!(
                "b = builtin.constant <builtin.integer <1: i1>> : {flag}; p = perry.alloc Unassigned (v, b) : {POINT};"
            ),
            "field y has a different type",
        ),
        (
            format!(
                "p = perry.alloc Unassigned (v, v) : {POINT}; z = perry.field_get p.\"z\" : builtin.fp64;"
            ),
            "has no field z",
        ),
        (
            format!("p = perry.alloc Scalar (v, v) : {POINT};"),
            "not an allocation tier",
        ),
        (
            format!(
                "p = perry.alloc Unassigned (v, v) : {POINT}; x = perry.field_get p.\"x\" : perry.string;"
            ),
            "field x has a different type",
        ),
        (
            format!(
                "p = perry.alloc Unassigned (v, v) : {POINT}; x = perry.field_present p.\"x\" : builtin.integer si1;"
            ),
            "field x has a different type",
        ),
        (
            "c = perry.compare Lt v, v : builtin.fp64;".to_string(),
            "must produce a signless i1",
        ),
    ];
    for (body, expected) in cases {
        let error = verify_text(&single_block(&body)).expect_err(&body);
        assert!(
            error.contains(expected),
            "{body}: expected {expected:?}, got {error}"
        );
    }
    verify_text(&single_block(&format!(
        "p = perry.alloc Task (v, v) : {POINT}; perry.field_set p.\"y\" = v; y = perry.field_get p.\"y\" : builtin.fp64;"
    )))
    .expect("well-typed heap ops verify");
    for (source, target, valid) in [
        ("perry.string", POINT, false),
        ("perry.exception", "builtin.fp64", false),
        ("perry.bigint", "builtin.fp64", false),
        ("builtin.integer si8", "builtin.fp64", true),
        ("perry.numeric", "builtin.fp64", true),
        (POINT, POINT, true),
    ] {
        let text = single_block(&format!("copy = perry.json_copy v : {target};"))
            .replacen("(builtin.fp64)", &format!("({source})"), 1)
            .replacen("v: builtin.fp64", &format!("v: {source}"), 1);
        let result = verify_text(&text);
        assert_eq!(result.is_ok(), valid, "{source} -> {target}: {result:?}");
        if let Err(error) = result {
            assert!(error.contains("json_copy"), "{error}");
        }
    }
    for fields in [vec![0, 0], vec![2]] {
        let ctx = &mut Context::new();
        let module = parse(
            ctx,
            &single_block(&format!("p = perry.alloc Task (v, v) : {POINT};")),
        );
        let alloc = Operation::get_op::<crate::AllocOp>(
            first::<crate::AllocOp>(ctx, &function(ctx, module, "f")),
            ctx,
        )
        .unwrap();
        alloc.set_attr_perry_field_order(ctx, crate::FieldOrder(fields));
        let error = verify_operation(module, ctx).expect_err("invalid field order");
        assert!(
            error
                .disp(ctx)
                .to_string()
                .contains("invalid property insertion order")
        );
    }
}

#[test]
fn record_redefinition_with_other_fields_is_rejected() {
    let body = "p = perry.alloc Unassigned (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; q = perry.alloc Unassigned (v) : perry.record <Point { x: builtin.fp64 }>;";
    let error = verify_text(&single_block(body)).expect_err("conflicting shapes");
    assert!(
        error.contains("already exists with different fields"),
        "{error}"
    );
}

#[test]
fn recursive_record_prints_by_name() -> Result<()> {
    let input = r#"
builtin.module @m {
  ^module():
    builtin.func @f: builtin.function <(perry.record <Node { value: builtin.fp64, next: perry.record <Node> }>) -> (builtin.fp64)> {
      ^entry(node: perry.record <Node>):
        next = perry.field_get node."next" : perry.record <Node>;
        value = perry.field_get next."value" : builtin.fp64;
        perry.return value
    }
}
"#;
    let ctx = &mut Context::new();
    let module = parse(ctx, input);
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    assert!(
        printed.contains("perry.record <Node { value: ")
            && printed.contains("next: perry.record <Node> }>"),
        "{printed}"
    );
    let f = function(ctx, module, "f");
    set_exported(ctx, &f);
    let returned = first::<ReturnOp>(ctx, &f);
    assert!(!facts(ctx, module)?.is_count(ctx, &f, returned.deref(ctx).get_operand(0)));
    Ok(())
}

#[test]
fn verifier_rejects_ill_typed_slot_access() {
    for access in [
        "perry.store s = b;",
        "x = perry.load s : perry.string;",
        "x = perry.global_load s : perry.string;",
    ] {
        let error = verify_text(&single_block(&format!(
            "s = perry.local : perry.slot <builtin.fp64>; b = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1; {access}",
        )))
        .expect_err("access with the wrong type");
        assert!(
            error.contains("value type differs from the slot"),
            "{error}"
        );
    }
}

/// `wasi.execution.scaling.retained_records` with a helper call, arrays and an optional read.
const RETAINED_RECORDS: &str = r#"
builtin.module @scaling {
  ^module():
    builtin.func @make: builtin.function <(builtin.fp64) -> (perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>)> {
      ^entry(i: builtin.fp64):
        two = builtin.constant <builtin.double 2.0> : builtin.fp64;
        doubled = perry.binary Mul i, two : builtin.fp64;
        point = perry.alloc Unassigned (i, doubled) : perry.record <Point>;
        perry.return point
    };
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.integer i32)> {
      ^entry(n: builtin.fp64):
        items = perry.array_new Unassigned () : perry.array <perry.record <Point>>;
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        perry.br ^header(zero)
      ^header(i: builtin.fp64):
        more = perry.compare Lt i, n : builtin.integer i1;
        perry.cond_br if more ^body() else ^exit()
      ^body():
        point = perry.call make (i) : perry.record <Point>;
        perry.array_push items <- point;
        one = builtin.constant <builtin.double 1.0> : builtin.fp64;
        next = perry.binary Add i, one : builtin.fp64;
        perry.br ^header(next)
      ^exit():
        count = perry.array_len items : builtin.integer i32;
        maybe = perry.array_get items[zero] : perry.optional <perry.record <Point>,Undefined>;
        first = perry.unwrap maybe : perry.record <Point>;
        perry.return count
    }
}
"#;

const ASYNC_STEP: &str = r#"
builtin.module @tasks {
  ^module():
    builtin.func @step: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(i: builtin.fp64):
        perry.return i
    };
    builtin.func @run: builtin.function <(builtin.fp64) -> (perry.string)> {
      ^entry(n: builtin.fp64):
        pending = perry.async_call step (n) : perry.promise <builtin.fp64>;
        value = perry.await pending : builtin.fp64;
        prefix = perry.string_const "done: " : perry.string;
        suffix = perry.string_const "漢🙂" : perry.string;
        text = perry.concat prefix, suffix : perry.string;
        length = perry.string_len text : builtin.integer i32;
        perry.return text
    }
}
"#;

#[test]
fn calls_arrays_strings_and_async_round_trip() -> Result<()> {
    let dictionary = single_block(
        "value = perry.undef : perry.array <perry.record <Renamed dict_tuple [ _0: perry.string, _1: builtin.fp64 ]>>;",
    );
    let recursive_collection = single_block(
        "value = perry.undef : perry.record <Recursive map_box [ _0: perry.array <perry.record <Entry map_tuple [ _0: perry.string, _1: perry.record <Recursive>, _2: builtin.integer i1, _3: builtin.fp64 ]>>, _1: builtin.fp64, _2: builtin.fp64, _3: builtin.fp64 ]>;",
    );
    for input in [
        RETAINED_RECORDS,
        ASYNC_STEP,
        &dictionary,
        &recursive_collection,
    ] {
        let ctx = &mut Context::new();
        let module = parse(ctx, input);
        round_trip(ctx, module)?;
    }
    Ok(())
}

#[test]
fn verifier_rejects_malformed_operation_contracts() {
    let mismatched_call = RETAINED_RECORDS.replace(
        "point = perry.call make (i) : perry.record <Point>;",
        "point = perry.call make (i, i) : perry.record <Point>;",
    );
    let error = verify_text(&mismatched_call).expect_err("argument count");
    assert!(
        error.contains("does not match the callee's signature"),
        "{error}"
    );

    let unknown = RETAINED_RECORDS.replace("perry.call make", "perry.call missing");
    let error = verify_text(&unknown).expect_err("unknown callee");
    assert!(
        error.contains("not a function of the enclosing module"),
        "{error}"
    );

    let wrong_await = ASYNC_STEP.replace(
        "value = perry.await pending : builtin.fp64;",
        "value = perry.await pending : perry.string;",
    );
    let error = verify_text(&wrong_await).expect_err("await type");
    assert!(
        error.contains("perry.promise of the result type"),
        "{error}"
    );
    let ctx = &mut Context::new();
    let number = FP64Type::get(ctx).into();
    let value = crate::UndefOp::new(ctx, number).get_result(ctx);
    for operands in [vec![], vec![value], vec![value, value]] {
        let malformed = Operation::new_op::<AwaitOp>(ctx, vec![number], operands, vec![], 0);
        let error =
            verify_operation(malformed.get_operation(), ctx).expect_err("invalid await source");
        assert!(
            error
                .disp(ctx)
                .to_string()
                .contains("perry.promise of the result type")
        );
    }

    let array = ArrayNewOp::new(ctx, number, vec![value]).get_result(ctx);
    for (operands, results) in [
        (vec![], vec![]),
        (vec![array, value, value], vec![]),
        (vec![array, value], vec![number]),
    ] {
        let malformed = Operation::new_op::<crate::ArrayPushOp>(ctx, results, operands, vec![], 0);
        assert!(
            malformed.verify(ctx).is_err(),
            "direct verification must reject malformed arity"
        );
        assert!(malformed.verify_interfaces(ctx).is_err());
    }
    let predicate = IntegerType::get(ctx, 1, Signedness::Signless).into();
    for (operands, results, valid) in [
        (vec![value, value], vec![predicate], true),
        (vec![value], vec![predicate], false),
        (vec![value, value], vec![], false),
        (vec![value, value], vec![predicate, predicate], false),
    ] {
        let compare = Operation::new_op::<CompareOp>(ctx, results, operands, vec![], 0);
        compare.set_attr_perry_compare_kind(ctx, CompareKind::Eq);
        assert_eq!(compare.verify(ctx).is_ok(), valid);
        assert_eq!(compare.verify_interfaces(ctx).is_ok(), valid);
    }
    let missing_result = Operation::new_op::<crate::WrapOp>(ctx, vec![], vec![value], vec![], 0);
    assert!(missing_result.verify(ctx).is_err());
    let missing_result = Operation::new_op::<ArrayNewOp>(ctx, vec![], vec![value], vec![], 0);
    assert!(missing_result.verify(ctx).is_err());
    assert!(
        crate::ConcatOp::from_sources(ctx, vec![])
            .verify(ctx)
            .is_err()
    );
    assert!(
        crate::ConcatSegmentsOp::new(ctx, value)
            .verify(ctx)
            .is_err()
    );

    let promises = "p = perry.promise_resolved v : perry.promise <builtin.fp64>; \
        ps = perry.array_new Unassigned (p) : perry.array <perry.promise <builtin.fp64>>; \
        settled = perry.undef : perry.settled <builtin.fp64>;";
    for (operation, result) in [
        (
            "perry.promise_combinator All ps",
            "perry.promise <perry.array <builtin.fp64>>",
        ),
        (
            "perry.promise_combinator Race ps",
            "perry.promise <builtin.fp64>",
        ),
        (
            "perry.promise_combinator AllSettled ps",
            "perry.promise <perry.array <perry.settled <builtin.fp64>>>",
        ),
        ("perry.settled_field Status settled", "perry.string"),
        (
            "perry.settled_field Value settled",
            "perry.optional <builtin.fp64,Undefined>",
        ),
        (
            "perry.settled_field Reason settled",
            "perry.optional <perry.exception,Undefined>",
        ),
        ("perry.promise_resolved v", "perry.promise <builtin.fp64>"),
        ("perry.promise_rejected v", "perry.promise <perry.string>"),
    ] {
        let invalid = if operation.contains("rejected") || !result.starts_with("perry.promise") {
            "builtin.fp64"
        } else {
            "perry.promise <perry.exception>"
        };
        for (ty, valid) in [(result, true), (invalid, false)] {
            let body = format!("{promises} r = {operation} : {ty};");
            assert_eq!(verify_text(&single_block(&body)).is_ok(), valid, "{body}");
        }
    }
    assert!(PromiseCombinatorOp::new(ctx, Combinator::All, value).is_none());
    assert!(crate::SettledFieldOp::new(ctx, crate::SettledField::Value, value).is_none());

    let status = crate::RaiseOp::new(ctx, value).get_result(ctx);
    assert!(crate::CatchOp::new(ctx, status).verify(ctx).is_ok());
    let catch = Operation::new_op::<crate::CatchOp>(ctx, vec![number], vec![status], vec![], 0);
    assert!(
        catch.verify(ctx).is_err(),
        "catch must produce an exception"
    );

    let wrong_push = RETAINED_RECORDS.replace(
        "perry.array_push items <- point;",
        "perry.array_push items <- i;",
    );
    let error = verify_text(&wrong_push).expect_err("element type");
    assert!(error.contains("element type differs"), "{error}");
}

#[test]
fn optional_reads_coerce_and_unwrap() {
    let numbers = "perry.array <builtin.fp64>";
    let maybe = "perry.optional <builtin.fp64,Undefined>";
    let read = format!(
        "a = perry.array_new Unassigned (v) : {numbers}; m = perry.array_get a[v] : {maybe};"
    );
    verify_text(&single_block(&format!(
        "{read} n = perry.to_number m : builtin.fp64; s = perry.binary Add n, v : builtin.fp64; \
         u = perry.is_undefined m : builtin.integer i1; w = perry.wrap v : {maybe}; \
         e = perry.compare Eq m, w : builtin.integer i1; d = perry.unwrap m : builtin.fp64; \
         x = perry.undefined : {maybe}; perry.array_set a[v] = d;"
    )))
    .expect("well-typed optional ops verify");

    for (payload, valid) in [("builtin.fp64", true), ("perry.string", false)] {
        let body = format!("x = perry.wrap v : perry.optional <{payload},Null>;");
        assert_eq!(verify_text(&single_block(&body)).is_ok(), valid, "{body}");
    }
    let cases = [
        (
            format!(
                "a = perry.array_new Unassigned (v) : {numbers}; m = perry.array_get a[v] : builtin.fp64;"
            ),
            "element type differs",
        ),
        (
            format!(
                "{read} b = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1; g = perry.array_get a[b] : {maybe};"
            ),
            "index must be a number",
        ),
        (
            format!("{read} n = perry.to_number v : builtin.fp64;"),
            "expects a perry.optional operand",
        ),
        (
            format!("{read} s = perry.to_string m : perry.string;"),
            "result type does not match",
        ),
        (
            "x = perry.undefined : builtin.fp64;".to_string(),
            "perry.undefined result type",
        ),
    ];
    for (body, expected) in cases {
        let error = verify_text(&single_block(&body)).expect_err(&body);
        assert!(
            error.contains(expected),
            "{body}: expected {expected:?}, got {error}"
        );
    }
    for (ty, width, signless) in [
        ("i1", 1, true),
        ("si1", 1, false),
        ("ui1", 1, false),
        ("i32", 32, true),
        ("si32", 32, false),
        ("ui32", 32, false),
    ] {
        for (operation, valid) in [
            ("perry.array_len a", width == 32),
            ("perry.string_len text", width == 32),
            ("perry.is_undefined m", width == 1 && signless),
            ("perry.is_null m", width == 1 && signless),
            ("perry.intrinsic NumberIsNaN (v)", width == 1),
            ("perry.raise v", width == 32 && signless),
        ] {
            let body = format!(
                "{read} text = perry.string_const \"\" : perry.string; x = {operation} : builtin.integer {ty};"
            );
            assert_eq!(
                verify_text(&single_block(&body)).is_ok(),
                valid,
                "{operation}: {ty}"
            );
        }
    }
    let numeric = "s = perry.undef : perry.string; n = perry.undef : perry.numeric; \
        i = perry.undef : builtin.integer si32; bytes = perry.undef : perry.bytes;";
    let optional_number = "perry.optional <builtin.fp64,Undefined>";
    for (operation, result, valid) in [
        ("BytesGet (bytes, v)", optional_number, true),
        (
            "BytesGet (bytes, v)",
            "perry.optional <builtin.fp64,Null>",
            true,
        ),
        ("BytesGet (bytes, v)", "builtin.fp64", false),
        ("BytesGet (bytes)", optional_number, false),
        ("BytesGet (bytes, v, v)", optional_number, false),
        ("BytesGet (bytes, n)", optional_number, false),
        ("StringCodePointAt (s, v)", optional_number, true),
        ("ParseInt (s, n)", "builtin.integer si32", true),
        ("ParseInt (s, i)", "perry.numeric", true),
        ("BytesAt (bytes, i)", "builtin.integer si32", true),
        ("BytesAt (bytes, i)", "perry.numeric", false),
        ("NumberToStringRadix (n, v)", "perry.string", true),
        ("NumberToStringRadix (v, n)", "perry.string", false),
        ("NumberInspect (n)", "perry.string", false),
        ("MathFloor (i)", "builtin.integer si32", true),
        ("MathFloor (i)", "builtin.integer si64", false),
        ("MathFloor (n)", "perry.numeric", true),
    ] {
        let body = format!("{numeric} r = perry.intrinsic {operation} : {result};");
        assert_eq!(verify_text(&single_block(&body)).is_ok(), valid, "{body}");
    }
}

#[test]
fn may_throw_marks_calls_unwraps_and_writes() {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &RETAINED_RECORDS.replace(
            "perry.return count",
            "perry.array_set items[zero] = first;\n\
             abs = perry.intrinsic MathAbs (count) : builtin.integer i32;\n\
             status = perry.caught : builtin.integer i32;\n\
             perry.return count",
        ),
    );
    verify_operation(module, ctx).expect("verifies");
    let mut throwing: Vec<String> = Vec::new();
    walk_op(
        ctx,
        &mut throwing,
        &WALKCONFIG_PREORDER_FORWARD,
        module,
        |ctx, throwing, node| {
            if let IRNode::Operation(op) = node {
                if Operation::is_op::<IntrinsicOp>(op, ctx) {
                    assert!(
                        !crate::may_throw(ctx, op),
                        "a pure intrinsic still has a status boundary"
                    );
                }
                if let Some(exception) = crate::semantics::exception(ctx, op) {
                    throwing.push(format!("{} {:?}", Operation::get_opid(op, ctx), exception));
                }
            }
        },
    );
    assert_eq!(
        throwing,
        [
            "perry.call Any",
            "perry.unwrap TypeError",
            "perry.array_set RangeError",
            "perry.intrinsic Any"
        ],
        "array_get, to_number and the rest are pure reads"
    );
}

#[test]
fn finite_variants_round_trip_and_verify_payload_types() -> Result<()> {
    let variant = r#"perry.variant <"empty", "n": builtin.fp64, "text": perry.string>"#;
    let input = single_block(&format!(
        r#"
        value = perry.variant_new "n" (v) : {variant};
        tag = perry.variant_tag value : perry.string;
        payload = perry.variant_payload value : builtin.fp64;
    "#
    ));
    let ctx = &mut Context::new();
    let module = parse(ctx, &input);
    round_trip(ctx, module)?;
    let invalid = single_block(&format!(
        r#"value = perry.variant_new "text" (v) : {variant};"#
    ));
    assert!(
        verify_text(&invalid)
            .unwrap_err()
            .contains("invalid tag or payload type")
    );
    Ok(())
}
