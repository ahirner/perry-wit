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
        point = perry.alloc Unassigned [0, 1] (i, doubled) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>;
        x = perry.field_get point.x : builtin.fp64;
        y = perry.field_get point.y : builtin.fp64;
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
        "perry.alloc Unassigned [0, 1] (",
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
fn byte_intrinsics_verify_storage_kinds_and_exception_effects() {
    let body = r#"
        buffer = perry.intrinsic BufferNew (v) : perry.buffer;
        created = perry.caught_status : builtin.integer i32;
        length = perry.wrap v : perry.optional <builtin.fp64>;
        bytes = perry.intrinsic BytesView (buffer, v, length) : perry.bytes;
        viewed = perry.caught_status : builtin.integer i32;
        byte = perry.intrinsic BytesGet (bytes, v) : perry.optional <builtin.fp64>;
        perry.intrinsic BytesSet (bytes, bytes, v) : ;
        copied = perry.caught_status : builtin.integer i32;
    "#;
    let input = single_block(body);
    verify_text(&input).expect("typed byte intrinsics verify");
    let invalid = input.replace("BytesView (buffer, v, length)", "BytesView (v, v, length)");
    assert!(
        verify_text(&invalid)
            .unwrap_err()
            .contains("incorrect operand or result types")
    );
    let infallible = single_block(
        "bytes = perry.intrinsic BytesNew (v) : perry.bytes; byte = perry.intrinsic BytesGet (bytes, v) : perry.optional <builtin.fp64>; status = perry.caught_status : builtin.integer i32;",
    );
    assert!(
        verify_text(&infallible)
            .unwrap_err()
            .contains("immediately follow a throwing operation")
    );
}

#[test]
fn verifier_rejects_ill_typed_heap_ops() {
    const POINT: &str = "perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>";
    let flag = "builtin.integer i1";
    let cases = [
        (
            format!("p = perry.alloc Unassigned [0, 1] (v) : {POINT};"),
            "has 2 fields but perry.alloc supplies 1",
        ),
        (
            format!(
                "b = builtin.constant <builtin.integer <1: i1>> : {flag}; p = perry.alloc Unassigned [0, 1] (v, b) : {POINT};"
            ),
            "field y has a different type",
        ),
        (
            format!(
                "p = perry.alloc Unassigned [0, 1] (v, v) : {POINT}; z = perry.field_get p.z : builtin.fp64;"
            ),
            "has no field z",
        ),
        (
            format!("p = perry.alloc Scalar [0, 1] (v, v) : {POINT};"),
            "not an allocation tier",
        ),
        (
            format!("p = perry.alloc Task [0, 0] (v, v) : {POINT};"),
            "property order must contain every field index exactly once",
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
        "p = perry.alloc Task [0, 1] (v, v) : {POINT}; perry.field_set p.y = v; y = perry.field_get p.y : builtin.fp64;"
    )))
    .expect("well-typed heap ops verify");
}

#[test]
fn record_redefinition_with_other_fields_is_rejected() {
    let body = "p = perry.alloc Unassigned [0, 1] (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; q = perry.alloc Unassigned [0, 1] (v) : perry.record <Point { x: builtin.fp64 }>;";
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
        next = perry.field_get node.next : perry.record <Node>;
        value = perry.field_get next.value : builtin.fp64;
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
fn dce_preserves_checked_reads_even_when_the_result_is_unused() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &single_block(
            "p = perry.alloc Unassigned [0, 1] (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; x = perry.field_get p.x : builtin.fp64; s = perry.binary Add x, v : builtin.fp64;",
        ),
    );
    assert_eq!(dce(module, ctx)?, IRStatus::Changed);
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    assert!(
        printed.contains("perry.alloc")
            && printed.contains("perry.field_get")
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
            "p = perry.alloc Unassigned [0, 1] (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; perry.field_set p.x = v;",
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
        point = perry.alloc Unassigned [0, 1] (i, doubled) : perry.record <Point>;
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
        maybe = perry.array_get items[zero] : perry.optional <perry.record <Point>>;
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
        text = perry.string_concat prefix, suffix : perry.string;
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
    let maybe = "perry.optional <builtin.fp64>";
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
            "perry.array_push TypeError",
            "perry.array_get TypeError",
            "perry.unwrap TypeError",
            "perry.array_set Any"
        ],
        "typed aggregate accesses can fail after JSON alias mutation"
    );
}

#[test]
fn exception_ops_round_trip_and_keep_caught_pairs() {
    let input = single_block(
        r#"
        value = perry.box_exception v : perry.exception;
        status = perry.raise value : builtin.integer i32;
        caught = perry.catch status : perry.exception;
        message = perry.exception_message caught : perry.optional <perry.string>;
        text = perry.unwrap message : perry.string;
        failure = perry.caught_status : builtin.integer i32;
        err = perry.error_new TypeError text : perry.exception;
        matches = perry.exception_is Any err : builtin.integer i1;
    "#,
    );
    let ctx = &mut Context::new();
    let module = parse(ctx, &input);
    verify_operation(module, ctx).unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    let printed = normalized(&print(ctx, module));
    verify_text(&printed).expect("printed exception ops verify");
    crate::pipeline::canonicalize(module, ctx).unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    let canonical = print(ctx, module);
    assert!(canonical.contains("perry.unwrap"));
    assert!(canonical.contains("perry.caught_status"));
    let unpaired = verify_text(&single_block(
        "s = perry.caught_status : builtin.integer i32;",
    ))
    .expect_err("unpaired status");
    assert!(
        unpaired.contains("immediately follow a throwing operation"),
        "{unpaired}"
    );
    let invalid = verify_text(&single_block("s = perry.raise v : builtin.integer i32;"))
        .expect_err("invalid thrown type");
    assert!(invalid.contains("raise needs an exception"), "{invalid}");
    verify_text(&single_block(
        "code = perry.intrinsic ExitCodeSet (v) : builtin.fp64; s = perry.caught_status : builtin.integer i32;",
    )).expect("checked intrinsic may throw");
    let infallible = verify_text(&single_block(
        "time = perry.intrinsic MonotonicNow () : builtin.fp64; s = perry.caught_status : builtin.integer i32;",
    )).expect_err("infallible intrinsic cannot produce a caught status");
    assert!(
        infallible.contains("immediately follow a throwing operation"),
        "{infallible}"
    );
}

#[test]
fn json_intrinsics_round_trip_and_reject_unchecked_scalar_uses() {
    let input = single_block(
        r#"
        text = perry.string_const "null" : perry.string;
        json = perry.intrinsic JsonParse (text) : perry.json;
        parsed = perry.caught_status : builtin.integer i32;
        optional = perry.wrap json : perry.optional <perry.json>;
        number = perry.intrinsic JsonAsNumber (optional) : builtin.fp64;
        narrowed = perry.caught_status : builtin.integer i32;
        encoded = perry.intrinsic JsonStringify (optional) : perry.optional <perry.string>;
        serialized = perry.caught_status : builtin.integer i32;
        formatted = perry.intrinsic JsonStringifyOptions (optional, optional, optional) : perry.optional <perry.string>;
        formatted_status = perry.caught_status : builtin.integer i32;
        reviver = perry.intrinsic JsonReviverStart (text) : perry.json_reviver;
        reviver_started = perry.caught_status : builtin.integer i32;
        ready = perry.intrinsic JsonReviverNext (reviver) : builtin.integer i1;
        advanced = perry.caught_status : builtin.integer i32;
        key = perry.intrinsic JsonReviverKey (reviver) : perry.string;
        value = perry.intrinsic JsonReviverValue (reviver) : perry.optional <perry.json>;
        context = perry.intrinsic JsonReviverContext (reviver) : perry.json;
        perry.intrinsic JsonReviverApply (reviver, value) : ;
        applied = perry.caught_status : builtin.integer i32;
        revived = perry.intrinsic JsonReviverResult (reviver) : perry.optional <perry.json>;
    "#,
    );
    verify_text(&input).expect("explicit JSON operations verify");
    let ctx = &mut Context::new();
    let module = parse(ctx, &input);
    verify_text(&normalized(&print(ctx, module))).expect("JSON operations round trip");
    let invalid = input.replace("JsonAsNumber (optional)", "JsonAsNumber (text)");
    assert!(
        verify_text(&invalid)
            .unwrap_err()
            .contains("incorrect operand or result types")
    );
    for invalid in [
        input.replace("JsonReviverNext (reviver)", "JsonReviverNext (json)"),
        input.replace("JsonReviverStart (text)", "JsonReviverStart (json)"),
        input.replace(
            "JsonReviverApply (reviver, value)",
            "JsonReviverApply (reviver, text)",
        ),
        input.replace(
            "JsonStringifyOptions (optional, optional, optional)",
            "JsonStringifyOptions (optional, text, optional)",
        ),
        input.replace(
            "JsonStringifyOptions (optional, optional, optional)",
            "JsonStringifyOptions (optional, optional)",
        ),
    ] {
        assert!(verify_text(&invalid).is_err());
    }
}

#[test]
fn native_json_views_verify_types_and_property_order() {
    let source = single_block(
        r#"
        point = perry.alloc Task [1, 0] (v, v) : perry.record <NativePoint { a: builtin.fp64, z: builtin.fp64 }>;
        json = perry.json_pack point : perry.json;
        typed = perry.json_unpack json : perry.record <NativePoint>;
        absent = perry.undefined : perry.optional <builtin.fp64>;
        optional = perry.json_pack absent : perry.optional <perry.json>;
    "#,
    );
    verify_text(&source).expect("native views are typed");
    let ctx = &mut Context::new();
    let module = parse(ctx, &source);
    let printed = normalized(&print(ctx, module));
    assert!(printed.contains("Task [1, 0]"), "{printed}");
    verify_text(&printed).expect("property order survives a round trip");
    let invalid = source.replace(
        "optional = perry.json_pack absent : perry.optional <perry.json>",
        "optional = perry.json_pack absent : perry.json",
    );
    assert!(verify_text(&invalid).is_err());
    let invalid = single_block(
        r#"
        bytes = perry.intrinsic BytesNew (v) : perry.bytes;
        status = perry.caught_status : builtin.integer i32;
        json = perry.json_pack bytes : perry.json;
    "#,
    );
    assert!(
        verify_text(&invalid)
            .unwrap_err()
            .contains("JSON-compatible")
    );
}

#[test]
fn dictionary_signatures_and_recursive_types_round_trip() -> Result<()> {
    let source = single_block(
        r#"
      d = perry.dictionary_new Unassigned : perry.dictionary <builtin.fp64>;
      key = perry.string_const "key" : perry.string;
      perry.dictionary Set (d, key, v) : ;
      value = perry.dictionary Get (d, key) : perry.optional <builtin.fp64>;
      keys = perry.dictionary Keys (d) : perry.array <perry.string>;
      values = perry.dictionary Values (d) : perry.array <builtin.fp64>;
      has = perry.dictionary Has (d, key) : builtin.integer i1;
      deleted = perry.dictionary Delete (d, key) : builtin.integer i1;
      recursive = perry.dictionary_new Unassigned : perry.dictionary <@Tree: perry.dictionary <@Tree>>;
      perry.dictionary Set (recursive, key, recursive) : ;
      mixed = perry.dictionary_new Unassigned : perry.dictionary <@Shared: perry.record <Shared { next: perry.dictionary <@Shared> }>>;
    "#,
    );
    let ctx = &mut Context::new();
    let module = parse(ctx, &source);
    verify_operation(module, ctx)?;
    let printed = normalized(&print(ctx, module));
    assert!(
        printed.contains("next:"),
        "nested record definition was lost: {printed}"
    );
    let next = &mut Context::new();
    let module = parse(next, &printed);
    verify_operation(module, next)?;
    assert_eq!(printed, normalized(&print(next, module)));
    for invalid in [
        source.replace("Set (d, key, v)", "Set (d, key, key)"),
        source.replace(
            "Get (d, key) : perry.optional <builtin.fp64>",
            "Get (d, key) : builtin.fp64",
        ),
        source.replace(
            "Keys (d) : perry.array <perry.string>",
            "Keys (d) : perry.array <builtin.fp64>",
        ),
        source.replace("Has (d, key)", "Has (d, v)"),
    ] {
        assert!(verify_text(&invalid).is_err());
    }
    Ok(())
}
