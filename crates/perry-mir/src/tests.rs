use pliron::{
    context::{Context, Ptr},
    irbuild::IRStatus,
    irfmt::parsers::spaced,
    operation::{Operation, verify_operation},
    opts::dce::dce,
    parsable::parse_from_str,
    printable::Printable,
    result::Result,
};

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

#[test]
fn temporary_record_loop_round_trips_and_verifies() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(ctx, TEMPORARY_RECORDS);
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
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
    let reparsed_ctx = &mut Context::new();
    // Printed `!N` location references point into an outline table that `disp` omits.
    let reparsed = parse(reparsed_ctx, &normalized(&printed));
    verify_operation(reparsed, reparsed_ctx)?;
    assert_eq!(
        normalized(&printed),
        normalized(&print(reparsed_ctx, reparsed))
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
            format!("p = perry.alloc Unassigned (v) : {POINT};"),
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
    Ok(())
}

#[test]
fn dce_removes_unused_allocation_and_reads() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &single_block(
            "p = perry.alloc Unassigned (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; x = perry.field_get p.\"x\" : builtin.fp64; s = perry.binary Add x, v : builtin.fp64;",
        ),
    );
    assert_eq!(dce(module, ctx)?, IRStatus::Changed);
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    assert!(
        !printed.contains("perry.alloc")
            && !printed.contains("perry.field_get")
            && !printed.contains("perry.binary"),
        "{printed}"
    );
    Ok(())
}

#[test]
fn dce_keeps_field_writes() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &single_block(
            "p = perry.alloc Unassigned (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; perry.field_set p.\"x\" = v;",
        ),
    );
    dce(module, ctx)?;
    let printed = print(ctx, module);
    assert!(
        printed.contains("perry.alloc") && printed.contains("perry.field_set"),
        "{printed}"
    );
    Ok(())
}

/// The sum loop as the builder emits it: mutable `let` bindings as slots, no SSA joins.
const SLOTTED_LOOP: &str = r#"
builtin.module @m {
  ^module():
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(n: builtin.fp64):
        i = perry.local : perry.slot <builtin.fp64>;
        sum = perry.local : perry.slot <builtin.fp64>;
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        perry.store i = zero;
        perry.store sum = zero;
        perry.br ^header()
      ^header():
        iv = perry.load i : builtin.fp64;
        more = perry.compare Lt iv, n : builtin.integer i1;
        perry.cond_br if more ^body() else ^exit()
      ^body():
        ib = perry.load i : builtin.fp64;
        sb = perry.load sum : builtin.fp64;
        next_sum = perry.binary Add sb, ib : builtin.fp64;
        perry.store sum = next_sum;
        one = builtin.constant <builtin.double 1.0> : builtin.fp64;
        next_i = perry.binary Add ib, one : builtin.fp64;
        perry.store i = next_i;
        perry.br ^header()
      ^exit():
        result = perry.load sum : builtin.fp64;
        perry.return result
    }
}
"#;

#[test]
fn mem2reg_turns_slots_into_block_arguments() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(ctx, SLOTTED_LOOP);
    verify_operation(module, ctx)?;
    let status =
        pliron::opts::mem2reg::mem2reg(module, ctx, &mut pliron::pass::AnalysisManager::default())?;
    assert_eq!(status, IRStatus::Changed);
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    for gone in ["perry.local", "perry.load", "perry.store"] {
        assert!(!printed.contains(gone), "{gone} survived:\n{printed}");
    }
    let header = printed
        .lines()
        .find(|line| line.trim_start().starts_with("^header"))
        .expect("header block");
    assert_eq!(
        header.matches("builtin.fp64").count(),
        2,
        "loop header must carry i and sum: {header}"
    );
    Ok(())
}

#[test]
fn verifier_rejects_ill_typed_slot_access() {
    let error = verify_text(&single_block(
        "s = perry.local : perry.slot <builtin.fp64>; b = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1; perry.store s = b;",
    ))
    .expect_err("store of the wrong type");
    assert!(
        error.contains("value type differs from the slot"),
        "{error}"
    );
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
    for input in [RETAINED_RECORDS, ASYNC_STEP] {
        let ctx = &mut Context::new();
        let module = parse(ctx, input);
        verify_operation(module, ctx)?;
        let printed = print(ctx, module);
        let reparsed_ctx = &mut Context::new();
        let reparsed = parse(reparsed_ctx, &normalized(&printed));
        verify_operation(reparsed, reparsed_ctx)?;
        assert_eq!(
            normalized(&printed),
            normalized(&print(reparsed_ctx, reparsed))
        );
    }
    Ok(())
}

#[test]
fn verifier_rejects_mismatched_calls_and_awaits() {
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
}

#[test]
fn may_throw_marks_calls_unwraps_and_writes() {
    use crate::exceptions::MayThrowInterface;
    use pliron::{
        graph::walkers::{
            IRNode, WALKCONFIG_PREORDER_FORWARD, uninterruptible::immutable::walk_op,
        },
        op::op_cast,
    };
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &RETAINED_RECORDS.replace(
            "perry.return count",
            "perry.array_set items[zero] = first;\n        perry.return count",
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
                let op = Operation::get_op_dyn(op, ctx);
                if let Some(may_throw) = op_cast::<dyn MayThrowInterface>(&*op) {
                    throwing.push(format!("{} {:?}", op.get_opid(), may_throw.exception(ctx)));
                }
            }
        },
    );
    assert_eq!(
        throwing,
        [
            "perry.call Any",
            "perry.unwrap TypeError",
            "perry.array_set RangeError"
        ],
        "array_get, to_number and the rest are pure reads"
    );
}

#[test]
fn scalar_replacement_removes_iteration_local_records() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(ctx, TEMPORARY_RECORDS);
    crate::pipeline::canonicalize(module, ctx)?;
    let printed = print(ctx, module);
    assert!(!printed.contains("perry.alloc"), "{printed}");
    assert!(!printed.contains("perry.field_get"), "{printed}");
    assert!(!printed.contains("perry.local"), "{printed}");
    Ok(())
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
    verify_operation(module, ctx)?;
    let printed = normalized(&print(ctx, module));
    let ctx = &mut Context::new();
    let reparsed = parse(ctx, &printed);
    verify_operation(reparsed, ctx)?;
    assert_eq!(printed, normalized(&print(ctx, reparsed)));
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
