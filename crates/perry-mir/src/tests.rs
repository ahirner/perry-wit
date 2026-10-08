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
                "p = perry.alloc Unassigned (v, v) : {POINT}; z = perry.field_get p.z : builtin.fp64;"
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
        "p = perry.alloc Task (v, v) : {POINT}; perry.field_set p.y = v; y = perry.field_get p.y : builtin.fp64;"
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
fn dce_removes_unused_allocation_and_reads() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        &single_block(
            "p = perry.alloc Unassigned (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; x = perry.field_get p.x : builtin.fp64; s = perry.binary Add x, v : builtin.fp64;",
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
            "p = perry.alloc Unassigned (v, v) : perry.record <Point { x: builtin.fp64, y: builtin.fp64 }>; perry.field_set p.x = v;",
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
