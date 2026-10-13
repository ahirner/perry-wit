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

#[test]
fn shared_liveness_distinguishes_edges_and_block_definitions() -> Result<()> {
    use pliron::{
        builtin::op_interfaces::OneRegionInterface, linked_list::ContainsLinkedList,
        pass::AnalysisManager,
    };
    let ctx = &mut Context::new();
    let root = parse(
        ctx,
        r#"
        builtin.module @live { ^module():
          builtin.func @run: builtin.function <(builtin.integer i1, builtin.fp64, builtin.fp64) -> (builtin.fp64)> {
            ^entry(flag: builtin.integer i1, x: builtin.fp64, y: builtin.fp64):
              perry.cond_br if flag ^left(x) else ^right()
            ^left(argument: builtin.fp64):
              sum = perry.binary Add argument, y : builtin.fp64;
              perry.return sum
            ^right():
              perry.return x
          }
        }
    "#,
    );
    verify_operation(root, ctx)?;
    let function = crate::aggregates::functions(root, ctx)[0];
    let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
    let args: Vec<_> = blocks[0].deref(ctx).arguments().collect();
    let branch = blocks[0].deref(ctx).get_terminator(ctx).unwrap();
    let addition = blocks[1].deref(ctx).iter(ctx).next().unwrap();
    let argument = blocks[1].deref(ctx).get_argument(0);
    let sum = addition.deref(ctx).get_result(0);
    let mut analyses = AnalysisManager::default();
    let live = analyses.get_analysis::<crate::liveness::LiveValues>(root, ctx)?;
    assert!(live.after(branch).contains(&args[1]));
    assert!(live.after(branch).contains(&args[2]));
    assert!(!live.after(branch).contains(&args[0]));
    assert!(!live.on_entry(blocks[1], args[1]));
    assert!(live.on_entry(blocks[1], args[2]));
    assert!(!live.on_entry(blocks[1], argument));
    assert!(live.on_entry(blocks[2], args[1]));
    assert_eq!(
        live.after(addition)
            .values()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([sum])
    );
    Ok(())
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
fn runtime_effects_propagate_recursion_mutation_escape_and_async_failure() -> Result<()> {
    use crate::{effects::Failure, facts::ProgramFacts};
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @effects {
  ^module():
    builtin.func @left: builtin.function <(perry.record <Cell { value: builtin.fp64 }>) -> (perry.record <Cell>)> {
      ^entry(cell: perry.record <Cell>):
        result = perry.call right (cell) : perry.record <Cell>;
        perry.return result
    };
    builtin.func @right: builtin.function <(perry.record <Cell>) -> (perry.record <Cell>)> {
      ^entry(cell: perry.record <Cell>):
        result = perry.call left (cell) : perry.record <Cell>;
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        maybe = perry.wrap zero : perry.optional <builtin.fp64,Undefined>;
        value = perry.unwrap maybe : builtin.fp64;
        perry.field_set result."value" = value;
        perry.return result
    };
    builtin.func @worker: builtin.function <(perry.record <Cell>) -> (perry.record <Cell>)> {
      ^entry(cell: perry.record <Cell>):
        result = perry.call left (cell) : perry.record <Cell>;
        perry.return result
    };
    builtin.func @launch: builtin.function <(perry.record <Cell>) -> (perry.promise <perry.record <Cell>>)> {
      ^entry(cell: perry.record <Cell>):
        pending = perry.async_call worker (cell) : perry.promise <perry.record <Cell>>;
        perry.return pending
    };
    builtin.func @wait: builtin.function <(perry.promise <perry.record <Cell>>) -> (perry.record <Cell>)> {
      ^entry(pending: perry.promise <perry.record <Cell>>):
        result = perry.await pending : perry.record <Cell>;
        perry.return result
    };
    builtin.func @rotate: builtin.function <(perry.record <Cell>, perry.record <Cell>) -> (perry.record <Cell>)> {
      ^entry(first: perry.record <Cell>, second: perry.record <Cell>):
        ignored = perry.call rotate (second, first) : perry.record <Cell>;
        perry.return first
    }
}
"#,
    );
    let functions = crate::aggregates::functions(module, ctx);
    let worker = functions
        .iter()
        .find(|f| f.get_symbol_name(ctx).as_ref() == "worker")
        .unwrap();
    crate::calls::set_async(ctx, worker);
    verify_operation(module, ctx)?;
    let mut analyses = AnalysisManager::default();
    let effects = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
    for name in ["left", "right", "worker", "launch"] {
        let summary = effects.effects(&name.try_into().unwrap()).unwrap();
        assert!(summary.effects.writes_record, "{name}");
        assert!(summary.escapes.contains(&0), "{name}");
        assert_eq!(
            summary.effects.failure,
            match name {
                "worker" => Failure::Rejects,
                "launch" => Failure::None,
                _ => Failure::Throws,
            },
            "{name}"
        );
    }
    let wait = effects.effects(&pliron::ident!("wait")).unwrap();
    assert!(wait.effects.may_mutate_records());
    assert_eq!(wait.effects.failure, Failure::Throws);
    let launch = effects.effects(&pliron::ident!("launch")).unwrap();
    assert!(launch.effects.allocates && launch.effects.may_collect && launch.effects.may_suspend);
    assert_eq!(
        effects.effects(&pliron::ident!("rotate")).unwrap().escapes,
        std::collections::BTreeSet::from([0, 1])
    );
    Ok(())
}

#[test]
fn runtime_effects_cache_tracks_mir_mutations() -> Result<()> {
    use crate::{effects::Failure, facts::ProgramFacts};
    use pliron::{
        builtin::op_interfaces::SymbolOpInterface,
        pass::{Analysis, AnalysisManager, PassResult},
    };
    let ctx = &mut Context::new();
    let module = parse(ctx, ASYNC_STEP);
    let mut analyses = AnalysisManager::default();
    let run = pliron::ident!("run");
    {
        let effects = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
        assert_eq!(
            effects.effects(&run).unwrap().effects.failure,
            Failure::Throws
        );
    }
    let function = crate::aggregates::functions(module, ctx)
        .into_iter()
        .find(|f| f.get_symbol_name(ctx) == run)
        .unwrap();
    crate::calls::set_async(ctx, &function);
    let mut changed = PassResult::default();
    changed.ir_changed = IRStatus::Changed;
    analyses.retain_preserved(&changed);
    assert!(analyses.try_get_analysis::<ProgramFacts>(module).is_none());
    let effects = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
    assert_eq!(
        effects.effects(&run).unwrap().effects.failure,
        Failure::Rejects
    );
    let fresh = ProgramFacts::compute(module, ctx, &mut AnalysisManager::default())?;
    for function in crate::aggregates::functions(module, ctx) {
        let name = function.get_symbol_name(ctx);
        assert_eq!(effects.effects(&name), fresh.effects(&name));
    }
    Ok(())
}

#[test]
fn program_facts_share_queries_and_scale_with_call_sites() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
    let mut visits = Vec::new();
    for calls in [32, 128] {
        let ctx = &mut Context::new();
        let mut text = String::from(
            r#"builtin.module @scaling { ^module():
          builtin.func @count: builtin.function <(perry.record <Counter { value: builtin.integer i32 }>, perry.string) -> (builtin.integer i32)> {
            ^entry(cell: perry.record <Counter>, text: perry.string):
              length = perry.string_len text : builtin.integer i32;
              perry.field_set cell."value" = length;
              perry.return length
          };
          builtin.func @run: builtin.function <(perry.record <Counter>, perry.string) -> (builtin.integer i32)> {
            ^entry(cell: perry.record <Counter>, text: perry.string):
        "#,
        );
        for index in 0..calls {
            use std::fmt::Write;
            writeln!(
                text,
                "value{index} = perry.call count (cell, text) : builtin.integer i32;"
            )
            .unwrap();
        }
        text.push_str(&format!("perry.return value{} }} }}", calls - 1));
        let root = parse(ctx, &text);
        verify_operation(root, ctx)?;
        let run = crate::aggregates::functions(root, ctx)
            .into_iter()
            .find(|f| f.get_symbol_name(ctx) == pliron::ident!("run"))
            .unwrap();
        let returned = run
            .get_entry_block(ctx)
            .deref(ctx)
            .get_terminator(ctx)
            .unwrap()
            .deref(ctx)
            .get_operand(0);
        let mut analyses = AnalysisManager::default();
        let identity = {
            let facts = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
            let effects = facts.effects(&pliron::ident!("run")).unwrap();
            assert!(effects.effects.writes_record);
            assert!(effects.escapes.is_empty());
            assert!(facts.is_count(ctx, &run, returned));
            visits.push(facts.constraint_visits);
            &*facts as *const ProgramFacts
        };
        let cached = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
        assert_eq!(identity, &*cached as *const ProgramFacts);
    }
    assert!(visits[1] < 5 * visits[0], "{visits:?}");
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
fn append_rewriting_expands_each_site_once_and_keeps_fallbacks() -> Result<()> {
    use pliron::{
        irbuild::match_rewrite::PassWrapper,
        pass::{AnalysisManager, Pass},
    };
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @appends {
  ^module():
    builtin.func @run: builtin.function <(perry.string, perry.string) -> (perry.string)> {
      ^entry(prefix: perry.string, suffix: perry.string):
        first = perry.concat_reuse [0] prefix, suffix : perry.string;
        second = perry.concat_reuse [0] first, suffix : perry.string;
        perry.return second
    }
}
"#,
    );
    let mut analyses = AnalysisManager::default();
    let mut pass = PassWrapper::new(
        "expand-appends",
        crate::fast_paths::ExpandAppends::default(),
    );
    assert_eq!(
        pass.run(module, ctx, &mut analyses)?.ir_changed,
        IRStatus::Changed
    );
    verify_operation(module, ctx)?;
    let printed = print(ctx, module);
    assert_eq!(
        printed.matches("perry.intrinsic string_try_append").count(),
        2,
        "{printed}"
    );
    assert_eq!(
        printed.matches("perry.concat_reuse").count(),
        2,
        "{printed}"
    );
    assert_eq!(printed.matches("perry.cond_br").count(), 2, "{printed}");
    assert_eq!(
        pass.run(module, ctx, &mut analyses)?.ir_changed,
        IRStatus::Unchanged
    );
    assert_eq!(print(ctx, module), printed);
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

#[test]
fn canonicalization_replaces_optional_users() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @optional {
  ^module():
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(v: builtin.fp64):
        wrapped = perry.wrap v : perry.optional <builtin.fp64,Undefined>;
        numeric = perry.to_number wrapped : builtin.fp64;
        unwrapped = perry.unwrap wrapped : builtin.fp64;
        sum = perry.binary Add numeric, unwrapped : builtin.fp64;
        perry.return sum
    }
}
"#,
    );
    crate::pipeline::canonicalize(module, ctx)?;
    let printed = print(ctx, module);
    for removed in ["perry.wrap", "perry.unwrap", "perry.to_number"] {
        assert!(!printed.contains(removed), "{removed} survived:\n{printed}");
    }
    assert!(
        normalized(&printed).contains("perry.binary Add v, v"),
        "{printed}"
    );
    Ok(())
}

#[test]
fn sccp_propagates_block_arguments_and_removes_constant_branches() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @constants {
  ^module():
    builtin.func @run: builtin.function <() -> (builtin.fp64)> {
      ^entry():
        two = builtin.constant <builtin.double 2.0> : builtin.fp64;
        perry.br ^calculate(two)
      ^calculate(x: builtin.fp64):
        four = perry.binary Mul x, x : builtin.fp64;
        take = perry.compare Gt four, x : builtin.integer i1;
        perry.cond_br if take ^yes(four) else ^no(x)
      ^yes(answer: builtin.fp64):
        perry.return answer
      ^no(wrong: builtin.fp64):
        perry.return wrong
    }
}

"#,
    );
    crate::pipeline::canonicalize(module, ctx)?;
    let printed = print(ctx, module);
    for removed in [
        "perry.binary",
        "perry.compare",
        "perry.cond_br",
        "^no",
        "^calculate",
        "^yes",
    ] {
        assert!(!printed.contains(removed), "{removed} survived:\n{printed}");
    }
    assert!(printed.contains("builtin.double 4>"), "{printed}");
    Ok(())
}

#[test]
fn sccp_keeps_loop_variants_and_folds_loop_invariants() -> Result<()> {
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @loop {
  ^module():
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(n: builtin.fp64):
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        seven = builtin.constant <builtin.double 7.0> : builtin.fp64;
        perry.br ^header(zero, seven)
      ^header(i: builtin.fp64, invariant: builtin.fp64):
        more = perry.compare Lt i, n : builtin.integer i1;
        perry.cond_br if more ^body() else ^exit()
      ^body():
        one = builtin.constant <builtin.double 1.0> : builtin.fp64;
        next = perry.binary Add i, one : builtin.fp64;
        perry.br ^header(next, invariant)
      ^exit():
        result = perry.binary Mul invariant, seven : builtin.fp64;
        perry.return result
    }
}
"#,
    );
    crate::pipeline::canonicalize(module, ctx)?;
    let printed = print(ctx, module);
    assert!(
        printed.contains("perry.cond_br"),
        "loop was erased:\n{printed}"
    );
    assert!(
        printed.contains("perry.binary Add"),
        "induction was folded:\n{printed}"
    );
    assert!(
        !printed.contains("perry.binary Mul"),
        "invariant was not folded:\n{printed}"
    );
    assert!(printed.contains("builtin.double 49>"), "{printed}");
    Ok(())
}

#[test]
fn sccp_folds_wrapping_integer_constants_at_each_width() {
    use crate::{attrs::BinaryKind, scalar::BinaryOp};
    use pliron::{
        builtin::{
            attributes::IntegerAttr,
            op_interfaces::OneResultInterface,
            ops::ConstantOp,
            types::{IntegerType, Signedness},
        },
        opts::constants::ConstFoldInterface,
        utils::apint::{APInt, bw},
    };
    let ctx = &mut Context::new();
    for width in [8, 16, 32, 64] {
        for signedness in [Signedness::Signed, Signedness::Unsigned] {
            let ty = IntegerType::get(ctx, width, signedness);
            let max = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            for (kind, lhs, rhs, expected) in [
                (BinaryKind::Add, max, 1, 0),
                (BinaryKind::Sub, 0, 1, max),
                (BinaryKind::Mul, max, 2, max - 1),
            ] {
                let lhs = IntegerAttr::new(ty, APInt::from_u64(lhs, bw(width as usize)));
                let rhs = IntegerAttr::new(ty, APInt::from_u64(rhs, bw(width as usize)));
                let a = ConstantOp::new(ctx, Box::new(lhs.clone())).get_result(ctx);
                let b = ConstantOp::new(ctx, Box::new(rhs.clone())).get_result(ctx);
                let op = BinaryOp::new(ctx, kind, a, b);
                let folded = op.check_fold(ctx, &[Some(Box::new(lhs)), Some(Box::new(rhs))]);
                let attr = folded[0]
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<IntegerAttr>()
                    .unwrap();
                assert_eq!(
                    attr.value().to_u64(),
                    expected,
                    "{kind:?} {signedness:?}{width}"
                );
            }
        }
    }
}

#[test]
fn sccp_preserves_floating_point_special_values() {
    use crate::{
        attrs::{BinaryKind, CompareKind},
        scalar::{BinaryOp, CompareOp},
    };
    use pliron::{
        builtin::{
            attributes::{FPDoubleAttr, IntegerAttr},
            op_interfaces::OneResultInterface,
            ops::ConstantOp,
        },
        opts::constants::ConstFoldInterface,
    };
    let ctx = &mut Context::new();
    for (kind, lhs, rhs, expected) in [
        (BinaryKind::Mul, -0.0, 2.0, -0.0),
        (BinaryKind::Div, 1.0, 0.0, f64::INFINITY),
        (BinaryKind::Rem, -5.0, 2.0, -1.0),
    ] {
        let lhs = FPDoubleAttr::from(lhs);
        let rhs = FPDoubleAttr::from(rhs);
        let a = ConstantOp::new(ctx, Box::new(lhs.clone())).get_result(ctx);
        let b = ConstantOp::new(ctx, Box::new(rhs.clone())).get_result(ctx);
        let op = BinaryOp::new(ctx, kind, a, b);
        let folded = op.check_fold(ctx, &[Some(Box::new(lhs)), Some(Box::new(rhs))]);
        let value = f64::from(
            folded[0]
                .as_ref()
                .unwrap()
                .downcast_ref::<FPDoubleAttr>()
                .unwrap()
                .clone(),
        );
        assert_eq!(value.to_bits(), expected.to_bits());
    }
    for kind in [
        CompareKind::Lt,
        CompareKind::Le,
        CompareKind::Gt,
        CompareKind::Ge,
        CompareKind::Eq,
        CompareKind::Ne,
    ] {
        let nan = FPDoubleAttr::from(f64::NAN);
        let a = ConstantOp::new(ctx, Box::new(nan.clone())).get_result(ctx);
        let op = CompareOp::new(ctx, kind, a, a);
        let folded = op.check_fold(ctx, &[Some(Box::new(nan.clone())), Some(Box::new(nan))]);
        let value = folded[0]
            .as_ref()
            .unwrap()
            .downcast_ref::<IntegerAttr>()
            .unwrap()
            .value();
        assert_eq!(!value.is_zero(), kind == CompareKind::Ne);
    }
}

#[test]
fn local_live_intervals_match_pliron_queries_through_loops() -> Result<()> {
    use pliron::{
        analyses::liveness::{Liveness, LivenessTq},
        builtin::op_interfaces::OneRegionInterface,
        graph::dominance::DomInfo,
        irbuild::inserter::OpInsertionPoint,
        linked_list::ContainsLinkedList,
        pass::AnalysisManager,
    };
    let ctx = &mut Context::new();
    let root = parse(ctx, TEMPORARY_RECORDS);
    let mut analyses = AnalysisManager::default();
    analyses.get_analysis::<crate::liveness::LiveValues>(root, ctx)?;
    let intervals = analyses
        .try_get_analysis::<crate::liveness::LiveValues>(root)
        .unwrap();
    let mut upstream = analyses
        .try_get_analysis_mut::<Liveness<LivenessTq>>(root)
        .unwrap();
    let mut dominance = analyses.try_get_analysis_mut::<DomInfo>(root).unwrap();
    for function in crate::aggregates::functions(root, ctx) {
        let mut values = Vec::new();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            values.extend(block.deref(ctx).arguments());
            for op in block.deref(ctx).iter(ctx) {
                values.extend(op.deref(ctx).results());
            }
        }
        for op in crate::aggregates::operations(ctx, &function) {
            for &value in &values {
                assert_eq!(
                    intervals.after(op).contains(&value),
                    upstream.is_live_at_point(
                        ctx,
                        &mut dominance,
                        value,
                        OpInsertionPoint::AfterOperation(op)
                    ),
                    "{value:?} after {op:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn atomic_construction_preserves_allocation_and_caller_collection_effects() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
    let ctx = &mut Context::new();
    let module = parse(
        ctx,
        r#"
builtin.module @atomic {
  ^module():
    builtin.func @left: builtin.function <(builtin.fp64) -> (perry.record <Cell { value: builtin.fp64 }>)> {
      ^entry(value: builtin.fp64):
        cell = perry.alloc Unassigned (value) : perry.record <Cell>;
        result = perry.call right (cell) : perry.record <Cell>;
        perry.return result
    };
    builtin.func @right: builtin.function <(perry.record <Cell>) -> (perry.record <Cell>)> {
      ^entry(cell: perry.record <Cell>):
        value = perry.field_get cell."value" : builtin.fp64;
        result = perry.call left (value) : perry.record <Cell>;
        perry.return result
    };
    builtin.func @run: builtin.function <(builtin.fp64) -> (perry.record <Cell>)> {
      ^entry(value: builtin.fp64):
        result = perry.call left (value) : perry.record <Cell>;
        perry.return result
    }
}
"#,
    );
    verify_operation(module, ctx)?;
    for function in crate::aggregates::functions(module, ctx) {
        if function.get_symbol_name(ctx).as_ref() != "run" {
            crate::calls::set_atomic_construction(ctx, &function);
        }
    }
    let mut analyses = AnalysisManager::default();
    let effects = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
    for name in ["left", "right"] {
        let effect = effects.effects(&name.try_into().unwrap()).unwrap();
        assert!(
            effect.effects.atomic_construction
                && effect.effects.allocates
                && effect.effects.requires_task()
        );
        assert!(
            !effect.effects.may_collect
                && !effect.effects.may_suspend
                && !effect.effects.may_reenter
        );
    }
    let effect = effects.effects(&pliron::ident!("run")).unwrap();
    assert!(
        !effect.effects.atomic_construction
            && effect.effects.allocates
            && effect.effects.may_collect
    );
    Ok(())
}

#[test]
fn atomic_construction_rejects_open_calls_and_observable_runtime_operations() {
    use crate::facts::ProgramFacts;
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
    let ordinary = r#"
builtin.module @atomic {
  ^module():
    builtin.func @leaf: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(value: builtin.fp64):
        perry.return value
    };
    builtin.func @run: builtin.function <(builtin.fp64) -> (builtin.fp64)> {
      ^entry(value: builtin.fp64):
        result = perry.call leaf (value) : builtin.fp64;
        perry.return result
    }
}
"#;
    let foreign = ordinary.replace("perry.call leaf", "perry.foreign Call \"demo#callback\"");
    let asynchronous = ordinary.replace(
        "result = perry.call leaf (value) : builtin.fp64;",
        "pending = perry.async_call leaf (value) : perry.promise <builtin.fp64>;\nresult = perry.await pending : builtin.fp64;",
    );
    for (text, operation) in [
        (ordinary, "perry.call"),
        (&foreign, "perry.foreign"),
        (&asynchronous, "perry.async_call"),
    ] {
        let ctx = &mut Context::new();
        let module = parse(ctx, text);
        let run = crate::aggregates::functions(module, ctx)
            .into_iter()
            .find(|f| f.get_symbol_name(ctx).as_ref() == "run")
            .unwrap();
        crate::calls::set_atomic_construction(ctx, &run);
        let mut analyses = AnalysisManager::default();
        let error = match analyses.get_analysis::<ProgramFacts>(module, ctx) {
            Ok(_) => panic!("accepted {operation} in an open construction group"),
            Err(error) => error.disp(ctx).to_string(),
        };
        assert!(error.contains(operation), "{error}");
        assert!(analyses.try_get_analysis::<ProgramFacts>(module).is_none());
    }
}

#[test]
fn atomic_construction_rejects_async_function_scope() {
    use crate::facts::ProgramFacts;
    use pliron::pass::AnalysisManager;
    let ctx = &mut Context::new();
    let module = parse(ctx, ASYNC_STEP);
    let function = crate::aggregates::functions(module, ctx)[0];
    crate::calls::set_async(ctx, &function);
    crate::calls::set_atomic_construction(ctx, &function);
    let mut analyses = AnalysisManager::default();
    let error = match analyses.get_analysis::<ProgramFacts>(module, ctx) {
        Ok(_) => panic!("accepted an asynchronous construction scope"),
        Err(error) => error.disp(ctx).to_string(),
    };
    assert!(error.contains("async function"), "{error}");
}
