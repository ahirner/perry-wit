use super::*;
use crate::{RecordType, heap::AllocOp};

/// The returned value of a single-return function.
fn returned(ctx: &Context, function: &FuncOp, index: usize) -> Value {
    first::<ReturnOp>(ctx, function)
        .deref(ctx)
        .get_operand(index)
}

#[test]
fn program_facts_share_queries_and_scale_with_call_sites() -> Result<()> {
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
            writeln!(
                text,
                "value{index} = perry.call count (cell, text) : builtin.integer i32;"
            )
            .unwrap();
        }
        text.push_str(&format!("perry.return value{} }} }}", calls - 1));
        let root = parse(ctx, &text);
        verify_operation(root, ctx)?;
        let run = function(ctx, root, "run");
        let returned = returned(ctx, &run, 0);
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
fn aggregate_facts_follow_calls_elements_and_aliased_writes() -> Result<()> {
    let fixture = r#"
builtin.module @aggregate_facts {
  ^module():
    builtin.func @make: builtin.function <(perry.string) -> (perry.record <Branch { child: perry.record <Leaf { count: builtin.fp64, stable: builtin.fp64 }> }>)> {
      ^entry(text: perry.string):
        length = perry.string_len text : builtin.integer i32;
        count = perry.numeric_cast length : builtin.fp64;
        leaf = perry.alloc Unassigned (count, count) : perry.record <Leaf>;
        branch = perry.alloc Unassigned (leaf) : perry.record <Branch>;
        perry.return branch
    };
    builtin.func @identity: builtin.function <(perry.record <Branch>, builtin.integer i1) -> (perry.record <Branch>)> {
      ^entry(branch: perry.record <Branch>, recurse: builtin.integer i1):
        perry.cond_br if recurse ^again() else ^done(branch)
      ^again():
        no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
        next = perry.call identity (branch, no) : perry.record <Branch>;
        perry.br ^done(next)
      ^done(result: perry.record <Branch>):
        perry.return result
    };
    builtin.func @write: builtin.function <(perry.record <Leaf>, builtin.fp64) -> ()> {
      ^entry(leaf: perry.record <Leaf>, value: builtin.fp64):
        perry.field_set leaf."count" = value;
        perry.return
    };
    builtin.func @run: builtin.function <(perry.string, builtin.fp64, builtin.integer i1) -> (builtin.fp64, builtin.fp64)> {
      ^entry(text: perry.string, value: builtin.fp64, recurse: builtin.integer i1):
        made = perry.call make (text) : perry.record <Branch>;
        branch = perry.call identity (made, recurse) : perry.record <Branch>;
        items = perry.array_new Unassigned (branch) : perry.array <perry.record <Branch>>;
        zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
        maybe = perry.array_get items[zero] : perry.optional <perry.record <Branch>,Undefined>;
        selected = perry.unwrap maybe : perry.record <Branch>;
        leaf = perry.field_get selected."child" : perry.record <Leaf>;
        MUTATE
        again = perry.field_get made."child" : perry.record <Leaf>;
        count = perry.field_get again."count" : builtin.fp64;
        stable = perry.field_get again."stable" : builtin.fp64;
        perry.return count, stable
    }
}
"#;
    for absence in ["", "perry.undefined", "perry.null"] {
        for (mutation, expected) in [
            ("", [true, true]),
            ("perry.call write (leaf, value) : ;", [false, true]),
            ("perry.call write (leaf, zero) : ;", [true, true]),
            (
                "view = perry.json_native made : perry.json;",
                [false, false],
            ),
            (
                "replacement = perry.alloc Unassigned (value, zero) : perry.record <Leaf>; perry.field_set selected.\"child\" = replacement;",
                [false, true],
            ),
        ] {
            let ctx = &mut Context::new();
            let mut input = fixture.replace("MUTATE", mutation);
            if !absence.is_empty() {
                input = input.replace("items = perry.array_new Unassigned (branch)", &format!(
                "present = perry.wrap branch : perry.optional <perry.record <Branch>,Both>; \
                 absent = {absence} : perry.optional <perry.record <Branch>,Both>; \
                 perry.cond_br if recurse ^join(present) else ^join(absent) \
                 ^join(chosen: perry.optional <perry.record <Branch>,Both>): \
                 unwrapped = perry.unwrap chosen : perry.record <Branch>; \
                 items = perry.array_new Unassigned (unwrapped)"
            ));
            }
            let module = parse(ctx, &input);
            verify_operation(module, ctx)?;
            let run = function(ctx, module, "run");
            set_exported(ctx, &run);
            let facts = facts(ctx, module)?;
            let returned = first::<ReturnOp>(ctx, &run);
            for (value, expected) in returned.deref(ctx).operands().zip(expected) {
                assert_eq!(facts.is_count(ctx, &run, value), expected, "{mutation}");
            }
        }
    }
    for ty in [
        "perry.record <Opaque>",
        "perry.array <perry.record <Opaque>>",
    ] {
        let ctx = &mut Context::new();
        let module = parse(
            ctx,
            &format!(
                r#"
builtin.module @opaque {{ ^module():
  builtin.func @run: builtin.function <({ty}) -> ({ty})> {{
    ^entry(value: {ty}): perry.return value
  }}
}}
"#
            ),
        );
        verify_operation(module, ctx)?;
        set_exported(ctx, &function(ctx, module, "run"));
        let mut analyses = AnalysisManager::default();
        let Err(error) = analyses.get_analysis::<ProgramFacts>(module, ctx) else {
            panic!("accepted unresolved heap schema {ty}");
        };
        assert!(error.to_string().contains("Opaque"), "{error}");
        assert!(analyses.try_get_analysis::<ProgramFacts>(module).is_none());
    }
    Ok(())
}

#[test]
fn array_lengths_follow_aliases_calls_and_growth() -> Result<()> {
    let fixture = r#"
builtin.module @lengths { ^module():
  builtin.func @identity: builtin.function <(perry.array <builtin.fp64>, builtin.integer i1) -> (perry.array <builtin.fp64>)> {
    ^entry(items: perry.array <builtin.fp64>, again: builtin.integer i1):
      perry.cond_br if again ^recurse() else ^done(items)
    ^recurse():
      no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
      result = perry.call identity (items, no) : perry.array <builtin.fp64>;
      perry.br ^done(result)
    ^done(value: perry.array <builtin.fp64>):
      perry.return value
  };
  builtin.func @write: builtin.function <(perry.array <builtin.fp64>, builtin.fp64) -> ()> {
    ^entry(alias: perry.array <builtin.fp64>, index: builtin.fp64):
      zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
      MUTATE
      perry.return
  };
  builtin.func @run: builtin.function <(builtin.integer i1, builtin.fp64) -> (builtin.integer i32, perry.optional <builtin.fp64,Undefined>)> {
    ^entry(flag: builtin.integer i1, index: builtin.fp64):
      one = builtin.constant <builtin.double 1.0> : builtin.fp64;
      zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
      items = perry.array_new Unassigned (one, one) : perry.array <builtin.fp64>;
      record = perry.alloc Unassigned (items) : perry.record <Holder { items: perry.array <builtin.fp64> }>;
      selected = perry.field_get record."items" : perry.array <builtin.fp64>;
      alias = perry.call identity (selected, flag) : perry.array <builtin.fp64>;
      length = perry.array_len items : builtin.integer i32;
      perry.call write (alias, index) : ;
      value = perry.array_get items[zero] : perry.optional <builtin.fp64,Undefined>;
      perry.return length, value
  }
}
"#;
    for (mutation, expected) in [
        ("", (2, 2)),
        ("perry.array_set alias[zero] = zero;", (2, 2)),
        ("perry.array_set alias[index] = zero;", (2, u32::MAX)),
        ("perry.array_push alias <- zero;", (2, u32::MAX)),
        ("extend", (2, u32::MAX)),
        (
            "view = perry.json_native alias : perry.json;",
            (0, u32::MAX),
        ),
    ] {
        let ctx = &mut Context::new();
        let module = parse(
            ctx,
            &fixture.replace("MUTATE", if mutation == "extend" { "" } else { mutation }),
        );
        if mutation == "extend" {
            let entry = function(ctx, module, "write").get_entry_block(ctx);
            let alias = entry.deref(ctx).get_argument(0);
            let end = entry.deref(ctx).get_terminator(ctx).unwrap();
            ArrayExtendOp::new(ctx, alias, alias)
                .get_operation()
                .insert_before(ctx, end);
        }
        verify_operation(module, ctx)?;
        let run = function(ctx, module, "run");
        set_exported(ctx, &run);
        let read = first::<ArrayGetOp>(ctx, &run);
        let array = read.deref(ctx).get_operand(0);
        let index = read.deref(ctx).get_operand(1);
        let mut analyses = AnalysisManager::default();
        {
            let cached = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
            let fresh = facts(ctx, module)?;
            for facts in [&*cached, &fresh] {
                assert_eq!(
                    facts.array_length(&run, array),
                    Some(expected),
                    "{mutation}"
                );
                assert_eq!(
                    facts.array_in_bounds(ctx, &run, array, index),
                    expected.0 > 0,
                    "{mutation}"
                );
            }
        }
        canonicalize(module, ctx)?;
        let (mut checked, mut length) = (false, false);
        for op in crate::aggregates::operations(ctx, &run) {
            checked |= Operation::is_op::<ArrayGetOp>(op, ctx);
            length |= Operation::is_op::<ArrayLenOp>(op, ctx);
        }
        assert!(!checked, "{mutation}");
        // An unused native view is eliminated before the fresh facts query.
        assert_eq!(
            length,
            expected.0 > 0 && expected.0 != expected.1,
            "{mutation}"
        );
    }
    Ok(())
}

#[test]
fn branch_bounds_require_stable_aliased_lengths() -> Result<()> {
    let fixture = r#"
builtin.module @bounds { ^module():
  builtin.func @shrink: builtin.function <(perry.array <builtin.fp64>) -> ()> {
    ^entry(items: perry.array <builtin.fp64>):
      zero = builtin.constant <builtin.integer <0: i32>> : builtin.integer i32;
      perry.array_truncate items, zero;
      perry.return
  };
  builtin.func @grow: builtin.function <(perry.array <builtin.fp64>) -> ()> {
    ^entry(items: perry.array <builtin.fp64>):
      zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
      perry.array_push items <- zero;
      perry.return
  };
  builtin.func @run: builtin.function <(perry.array <builtin.fp64>) -> (perry.optional <builtin.fp64,Undefined>)> {
    ^entry(items: perry.array <builtin.fp64>):
      zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
      count = builtin.constant <builtin.integer <0: i32>> : builtin.integer i32;
      other = perry.array_new Unassigned (zero) : perry.array <builtin.fp64>;
      holder = perry.alloc Unassigned (items) : perry.record <Holder { items: perry.array <builtin.fp64> }>;
      alias = perry.field_get holder."items" : perry.array <builtin.fp64>;
      length = perry.array_len items : builtin.integer i32;
      bound = perry.numeric_cast length : builtin.fp64;
      BEFORE
      more = perry.compare Lt zero, bound : builtin.integer i1;
      perry.cond_br if more ^body() else ^empty()
    ^body():
      DURING
      value = perry.array_get items[zero] : perry.optional <builtin.fp64,Undefined>;
      AFTER
      perry.return value
    ^empty():
      absent = perry.undefined : perry.optional <builtin.fp64,Undefined>;
      perry.return absent
  }
}
"#;
    for (position, mutation, checked) in [
        ("DURING", "perry.array_truncate items, count;", true),
        ("DURING", "perry.array_truncate alias, count;", true),
        ("BEFORE", "perry.array_truncate alias, count;", true),
        ("DURING", "perry.call shrink (holder) : ;", true),
        ("DURING", "perry.call shrink (alias) : ;", true),
        ("DURING", "perry.call shrink (other) : ;", false),
        ("DURING", "perry.array_truncate other, count;", false),
        ("DURING", "perry.array_push alias <- zero;", false),
        ("DURING", "perry.call grow (alias) : ;", false),
        ("AFTER", "perry.array_truncate alias, count;", false),
        ("LOOP", "perry.array_truncate alias, count;", true),
        ("REFRESH", "perry.array_truncate alias, count;", false),
        ("DURING", "", false),
    ] {
        let ctx = &mut Context::new();
        let repeats = matches!(position, "LOOP" | "REFRESH");
        let mut input = fixture.replace(if repeats { "AFTER" } else { position }, mutation);
        if repeats {
            let header = if position == "REFRESH" {
                "length = perry.array_len"
            } else {
                "more = perry.compare"
            };
            input = input
                .replace(
                    header,
                    &format!("perry.br ^header()\n    ^header():\n      {header}"),
                )
                .replace("perry.return value", "perry.br ^header()");
        }
        for unused in ["BEFORE", "DURING", "AFTER"] {
            input = input.replace(unused, "");
        }
        // The record case checks mutation through a containing argument.
        if mutation.contains("(holder)") {
            input = input.replace(
                "@shrink: builtin.function <(perry.array <builtin.fp64>)",
                "@shrink: builtin.function <(perry.record <Holder { items: perry.array <builtin.fp64> }>)",
            ).replace(
                "^entry(items: perry.array <builtin.fp64>):\n      zero = builtin.constant <builtin.integer",
                "^entry(record: perry.record <Holder>):\n      items = perry.field_get record.\"items\" : perry.array <builtin.fp64>;\n      zero = builtin.constant <builtin.integer",
            );
        }
        let module = parse(ctx, &input);
        verify_operation(module, ctx)?;
        let run = function(ctx, module, "run");
        set_exported(ctx, &run);
        let numeric = facts(ctx, module)?;
        let mut dominance = DomInfo::default();
        crate::ranges::prove_bounds(module, ctx, &numeric, &mut dominance);
        let retained = crate::aggregates::operations(ctx, &run)
            .into_iter()
            .any(|op| Operation::is_op::<ArrayGetOp>(op, ctx));
        assert_eq!(retained, checked, "{position}: {mutation}");
        verify_operation(module, ctx)?;
    }
    Ok(())
}

#[test]
fn json_copy_paths_preserve_parse_order_without_visiting_dead_ends() -> Result<()> {
    let fixture = r#"
builtin.module @paths { ^module():
  builtin.func @effect: builtin.function <() -> ()> {
    ^entry(): perry.return
  };
  builtin.func @run: builtin.function <(builtin.integer i1) -> (builtin.fp64)> {
    ^entry(flag: builtin.integer i1):
      zero = builtin.constant <builtin.double 0.0> : builtin.fp64;
      items = perry.array_new Unassigned (zero) : perry.array <builtin.fp64>;
      other = perry.array_new Unassigned (zero) : perry.array <builtin.fp64>;
      text = perry.json_stringify items : perry.string;
      perry.cond_br if flag ^before() else ^parse()
    ^before():
      BEFORE
      perry.br ^parse()
    ^parse():
      parsed = perry.intrinsic JsonParse (text) : perry.json;
      perry.cond_br if flag ^after() else ^side()
    ^after():
      AFTER
      perry.br ^consume()
    ^side():
      SIDE
      EXIT
    ^consume():
      number = perry.intrinsic JsonToNumber (parsed) : builtin.fp64;
      TAIL
      perry.return number
  }
}
"#;
    for (position, operation, blocks) in [
        ("BEFORE", "perry.call effect () : ;", false),
        ("AFTER", "perry.call effect () : ;", true),
        ("BEFORE", "perry.array_push items <- zero;", true),
        ("AFTER", "perry.array_push other <- zero;", false),
        ("SIDE", "perry.array_push items <- zero;", false),
        ("TAIL", "perry.array_push items <- zero;", true),
    ] {
        let ctx = &mut Context::new();
        let mut input = fixture.replace(position, operation);
        for unused in ["BEFORE", "AFTER", "SIDE", "TAIL"] {
            input = input.replace(unused, "");
        }
        input = input.replace(
            "EXIT",
            if position == "SIDE" {
                "perry.return zero"
            } else {
                "perry.br ^consume()"
            },
        );
        if position == "TAIL" {
            input = input.replace(
                "perry.return number",
                "perry.cond_br if flag ^consume() else ^done()\n    ^done(): perry.return number",
            );
        }
        let module = parse(ctx, &input);
        verify_operation(module, ctx)?;
        let run = function(ctx, module, "run");
        let stringify = first::<crate::JsonStringifyOp>(ctx, &run);
        let intrinsic = |kind| {
            crate::aggregates::operations(ctx, &run)
                .into_iter()
                .find(|&op| crate::intrinsics::intrinsic_of(ctx, op) == Some(kind))
                .expect("fixture intrinsic")
        };
        let parse_op = intrinsic(crate::Intrinsic::JsonParse);
        let consumer = intrinsic(crate::Intrinsic::JsonToNumber);
        assert_eq!(
            facts(ctx, module)?.may_prevent_json_copy_between(
                ctx,
                &run,
                stringify.deref(ctx).get_operand(0),
                stringify,
                (None, Some(parse_op)),
                consumer,
            ),
            blocks,
            "{position}: {operation}"
        );
    }
    Ok(())
}

#[test]
fn promise_outcomes_follow_aggregate_aliases_and_recursive_calls() -> Result<()> {
    let fixture = r#"
builtin.module @outcomes { ^module():
  builtin.func @identity: builtin.function <(perry.promise <builtin.fp64>, builtin.integer i1) -> (perry.promise <builtin.fp64>)> {
    ^entry(job: perry.promise <builtin.fp64>, again: builtin.integer i1):
      perry.cond_br if again ^recurse() else ^done(job)
    ^recurse():
      no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
      recursive = perry.call identity (job, no) : perry.promise <builtin.fp64>;
      perry.br ^done(recursive)
    ^done(result: perry.promise <builtin.fp64>):
      perry.return result
  };
  builtin.func @write: builtin.function <(perry.record <Holder { jobs: perry.array <perry.promise <builtin.fp64>> }>, perry.promise <builtin.fp64>) -> ()> {
    ^entry(holder: perry.record <Holder>, replacement: perry.promise <builtin.fp64>):
      jobs = perry.array_new Unassigned (replacement) : perry.array <perry.promise <builtin.fp64>>;
      perry.field_set holder."jobs" = jobs;
      perry.return
  };
  builtin.func @run: builtin.function <(perry.promise <builtin.fp64>, builtin.integer i1) -> (builtin.fp64)> {
    ^entry(unknown: perry.promise <builtin.fp64>, again: builtin.integer i1):
      one = builtin.constant <builtin.double 1.0> : builtin.fp64;
      ready = perry.promise_resolved one : perry.promise <builtin.fp64>;
      failed = perry.promise_rejected one : perry.promise <builtin.fp64>;
      jobs = perry.array_new Unassigned (ready) : perry.array <perry.promise <builtin.fp64>>;
      holder = perry.alloc Unassigned (jobs) : perry.record <Holder>;
      MUTATE
      zero = builtin.constant <builtin.integer <0: i32>> : builtin.integer i32;
      alias = perry.field_get holder."jobs" : perry.array <perry.promise <builtin.fp64>>;
      picked = perry.array_at alias[zero] : perry.promise <builtin.fp64>;
      pending = perry.call identity (picked, again) : perry.promise <builtin.fp64>;
      value = perry.await pending : builtin.fp64;
      HANDLE
      perry.return value
  }
}
"#;
    for handled in [false, true] {
        for (mutation, rejects) in [
            ("", false),
            ("perry.call write (holder, ready) : ;", false),
            ("perry.call write (holder, failed) : ;", true),
            ("perry.call write (holder, unknown) : ;", true),
            (
                "extra = perry.array_new Unassigned (failed) : perry.array <perry.promise <builtin.fp64>>; perry.array_extend jobs, extra;",
                true,
            ),
            (
                "extra = perry.array_new Unassigned (failed) : perry.array <perry.promise <builtin.fp64>>; copied = perry.concat jobs, extra : perry.array <perry.promise <builtin.fp64>>; perry.field_set holder.\"jobs\" = copied;",
                true,
            ),
            (
                "extra = perry.array_new Unassigned (ready) : perry.array <perry.promise <builtin.fp64>>; segments = perry.array_new Unassigned (jobs, extra) : perry.array <perry.array <perry.promise <builtin.fp64>>>; copied = perry.concat_segments segments : perry.array <perry.promise <builtin.fp64>>; perry.field_set holder.\"jobs\" = copied; perry.array_push extra <- failed;",
                true,
            ),
            ("view = perry.json_native holder : perry.json;", true),
        ] {
            let ctx = &mut Context::new();
            let text = fixture.replace("MUTATE", mutation).replace(
                "HANDLE",
                if handled {
                    "status = perry.caught : builtin.integer i32;"
                } else {
                    ""
                },
            );
            let root = parse(ctx, &text);
            let run = function(ctx, root, "run");
            set_async(ctx, &run);
            set_exported(ctx, &run);
            verify_operation(root, ctx)?;
            let pending = first::<AwaitOp>(ctx, &run).deref(ctx).get_operand(0);
            let mut analyses = AnalysisManager::default();
            let cached = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
            let fresh = facts(ctx, root)?;
            let expected = if rejects {
                Failure::Rejects { escapes: !handled }
            } else {
                Failure::None
            };
            for facts in [&*cached, &fresh] {
                assert_eq!(facts.may_reject(&run, pending), rejects, "{mutation}");
                let failure = facts
                    .effects(&pliron::ident!("run"))
                    .unwrap()
                    .effects
                    .failure;
                assert_eq!(failure, expected, "{mutation}");
            }
        }
    }
    Ok(())
}

#[test]
fn combinator_outcomes_join_inputs_without_mutating_them() -> Result<()> {
    for kind in [Combinator::All, Combinator::Race, Combinator::AllSettled] {
        for rejected in [false, true] {
            let ctx = &mut Context::new();
            let text = format!(
                r#"builtin.module @combine {{ ^module():
              builtin.func @run: builtin.function <() -> (builtin.fp64)> {{ ^entry():
                one = builtin.constant <builtin.double 1.0> : builtin.fp64;
                job = perry.promise_{} one : perry.promise <builtin.fp64>;
                jobs = perry.array_new Unassigned (job) : perry.array <perry.promise <builtin.fp64>>;
                perry.return one
              }}
            }}"#,
                if rejected { "rejected" } else { "resolved" }
            );
            let root = parse(ctx, &text);
            let run = function(ctx, root, "run");
            set_async(ctx, &run);
            set_exported(ctx, &run);
            let array = first::<ArrayNewOp>(ctx, &run).deref(ctx).get_result(0);
            let end = first::<ReturnOp>(ctx, &run);
            let group = PromiseCombinatorOp::new(ctx, kind, array).unwrap();
            group.get_operation().insert_before(ctx, end);
            let pending = group.get_result(ctx);
            let wait = AwaitOp::new(ctx, pending)?;
            wait.get_operation().insert_before(ctx, end);
            verify_operation(root, ctx)?;
            let facts = facts(ctx, root)?;
            assert_eq!(
                facts.may_reject(&run, pending),
                rejected && kind != Combinator::AllSettled
            );
            assert_eq!(facts.array_length(&run, array), Some((1, 1)));
        }
    }
    Ok(())
}

#[test]
fn aggregate_summary_bounds_preserve_existing_alias_soundness() -> Result<()> {
    let mut visits = Vec::new();
    for size in [8, 32, 128, 512] {
        let ctx = &mut Context::new();
        let mut text = String::from(
            r#"builtin.module @origins { ^module():
          builtin.func @run: builtin.function <() -> (builtin.fp64)> { ^entry():
            one = builtin.constant <builtin.double 1.0> : builtin.fp64;
            first = perry.alloc Unassigned (one) : perry.record <Cell { value: builtin.fp64 }>;
            items = perry.array_new Unassigned (first) : perry.array <perry.record <Cell>>;
        "#,
        );
        for index in 1..size {
            writeln!(text, "cell{index} = perry.alloc Unassigned (one) : perry.record <Cell>; perry.array_push items <- cell{index}; length{index} = perry.array_len items : builtin.integer i32;").unwrap();
        }
        text.push_str(
            r#"result = perry.field_get first."value" : builtin.fp64; perry.return result } }"#,
        );
        let module = parse(ctx, &text);
        verify_operation(module, ctx)?;
        let run = function(ctx, module, "run");
        let facts = facts(ctx, module)?;
        let array = first::<ArrayNewOp>(ctx, &run).deref(ctx).get_result(0);
        assert_eq!(facts.array_length(&run, array), Some((1, u32::MAX)));
        assert_eq!(
            facts.is_count(ctx, &run, returned(ctx, &run, 0)),
            size <= 32
        );
        assert_eq!(facts.summarized_heap_sets > 0, size > 32);
        visits.push(facts.constraint_visits);
    }
    for pair in visits.windows(2) {
        assert!(pair[1] < 5 * pair[0], "{visits:?}");
    }
    Ok(())
}

/// Precise per-arity context accounting; integration tests only check that summarized
/// compilation stays sound.
#[test]
fn context_summarization_bounds_mixed_integer_and_float_arguments() -> Result<()> {
    // Every u8/f64 mix of two parameters: 4 distinct contexts, twice the precise budget of 2.
    let arity = 2;
    let ctx = &mut Context::new();
    let mut text = String::from(
        r#"builtin.module @test { ^module():
              builtin.func @helper: builtin.function <(builtin.fp64, builtin.fp64) -> (builtin.fp64)> {
              ^entry(p0: builtin.fp64, p1: builtin.fp64):
                acc = perry.binary Add p0, p1 : builtin.fp64;
                perry.return acc
              }"#,
    );
    let combos = 1usize << arity;
    for mask in 0..combos {
        writeln!(
            text,
            r#"; builtin.func @e{mask}: builtin.function <(builtin.fp64, builtin.fp64) -> (builtin.fp64)> {{
                  ^entry(p0: builtin.fp64, p1: builtin.fp64):
                    res = perry.call helper (p0, p1) : builtin.fp64;
                    perry.return res
                }}"#
        )
        .unwrap();
    }
    text.push('}');
    let module = parse(ctx, &text);
    verify_operation(module, ctx)?;
    for mask in 0..combos {
        let entry = function(ctx, module, &format!("e{mask}"));
        set_exported(ctx, &entry);
        let args = (0..arity)
            .map(|bit| {
                if (mask & (1 << bit)) != 0 {
                    IntegerType::get(ctx, 8, Signedness::Unsigned).into()
                } else {
                    FP64Type::get(ctx).into()
                }
            })
            .collect();
        let results = vec![FP64Type::get(ctx).into()];
        constrain_signature(ctx, &entry, args, results);
    }
    let facts = with_fact_limits(2, 2, || facts(ctx, module))?;
    // One generic context, 2 precise ones and one shared summary.
    assert_eq!(facts.context_counts(&pliron::ident!("helper")), (1, 3));
    assert_eq!(facts.summarized_contexts, combos - 2);
    Ok(())
}

/// A recursive helper returning `(status, x / 2, x + x)` to a `u8 -> (bool, f64, u32)` export:
/// each call result takes its own domain instead of one shared representation.
#[test]
fn specialization_keeps_each_call_result_independent() -> Result<()> {
    let ctx = &mut Context::new();
    let root = parse(
        ctx,
        r#"builtin.module @multiple_results { ^module():
            builtin.func @helper: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.integer i1, builtin.fp64, builtin.fp64)> {
              ^entry(x: builtin.fp64, recurse: builtin.integer i1):
                perry.cond_br if recurse ^recursive() else ^base()
              ^recursive():
                no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
                status, fraction, sum = perry.call helper (x, no) : builtin.integer i1, builtin.fp64, builtin.fp64;
                perry.br ^done(status, fraction, sum)
              ^base():
                yes = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1;
                two = builtin.constant <builtin.double 2.0> : builtin.fp64;
                half = perry.binary Div x, two : builtin.fp64;
                doubled = perry.binary Add x, x : builtin.fp64;
                perry.br ^done(yes, half, doubled)
              ^done(ok: builtin.integer i1, divided: builtin.fp64, added: builtin.fp64):
                perry.return ok, divided, added
            };
            builtin.func @run: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.integer i1, builtin.fp64, builtin.fp64)> {
              ^entry(x: builtin.fp64, recurse: builtin.integer i1):
                status, fraction, sum = perry.call helper (x, recurse) : builtin.integer i1, builtin.fp64, builtin.fp64;
                perry.return status, fraction, sum
            }
        }"#,
    );
    let run = function(ctx, root, "run");
    let boolean: TypeHandle = IntegerType::get(ctx, 1, Signedness::Signless).into();
    let byte = IntegerType::get(ctx, 8, Signedness::Unsigned).into();
    let wide: TypeHandle = IntegerType::get(ctx, 32, Signedness::Unsigned).into();
    let float: TypeHandle = FP64Type::get(ctx).into();
    constrain_signature(ctx, &run, vec![byte, boolean], vec![boolean, float, wide]);
    set_exported(ctx, &run);
    canonicalize(root, ctx)?;
    let results = |ctx: &Context, name: &str| -> Result<Vec<TypeHandle>> {
        let f = function(ctx, root, name);
        Ok(
            TypedHandle::<FunctionType>::from_handle(f.get_type(ctx), ctx)?
                .deref(ctx)
                .res_types(),
        )
    };
    let implementation = specialized_implementation(ctx, &run);
    assert_ne!(implementation, pliron::ident!("run"));
    assert_eq!(
        results(ctx, implementation.as_ref())?,
        [boolean, float, wide]
    );
    // The unconstrained export keeps number semantics for every result.
    assert_eq!(results(ctx, "run")?, [boolean, float, float]);
    Ok(())
}

#[test]
fn lossless_representation_verifier_rejects_precision_degradation() -> Result<()> {
    let ctx = &mut Context::new();
    let text = r#"
builtin.module @lossless_check { ^module():
  builtin.func @run: builtin.function <(perry.string) -> (perry.record <Node { x: builtin.fp64, next: perry.optional <perry.record <Node>,Undefined> }>)> {
    ^entry(input: perry.string):
      ten = builtin.constant <builtin.double 10.0> : builtin.fp64;
      parsed = perry.intrinsic ParseInt (input, ten) : builtin.fp64;
      none = perry.undefined : perry.optional <perry.record <Node>,Undefined>;
      node = perry.alloc Unassigned (parsed, none) : perry.record <Node>;
      loaded = perry.field_get node."x" : builtin.fp64;
      copy = perry.alloc Unassigned (loaded, none) : perry.record <Node>;
      linked = perry.wrap node : perry.optional <perry.record <Node>,Undefined>;
      back = perry.wrap copy : perry.optional <perry.record <Node>,Undefined>;
      perry.field_set copy."next" = linked;
      perry.field_set node."next" = back;
      perry.return copy
  }
}
"#;
    let module = parse(ctx, text);
    let run = function(ctx, module, "run");
    set_exported(ctx, &run);
    let orig_ret_ty = TypedHandle::<FunctionType>::from_handle(run.get_type(ctx), ctx)?
        .deref(ctx)
        .res_types()[0];
    verify_operation(module, ctx)?;

    let mut analyses = AnalysisManager::default();
    Specialize.run(module, ctx, &mut analyses)?;
    verify_operation(module, ctx)?;
    let facts = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
    verify_lossless_module(module, ctx, &facts)?;

    let spec_fn = function(ctx, module, "run");
    let layout = first::<AllocOp>(ctx, &spec_fn)
        .deref(ctx)
        .get_result(0)
        .get_type(ctx);
    let layout_name = TypedHandle::<RecordType>::from_handle(layout, ctx)?
        .deref(ctx)
        .name()
        .clone();
    let record = TypedHandle::<RecordType>::from_handle(layout, ctx)?;
    let (_, next) = record.deref(ctx).field("next").unwrap();
    assert_eq!(crate::optional_value(ctx, next), Some(layout));
    let fp64: TypeHandle = FP64Type::get(ctx).into();

    // 1. A ParseInt result and 2. a FieldGetOp of a Fact::Numeric field degraded back to f64
    // must fail lossless verification.
    for degraded in [
        first::<IntrinsicOp>(ctx, &spec_fn),
        first::<FieldGetOp>(ctx, &spec_fn),
    ] {
        let result = degraded.deref(ctx).get_result(0);
        let saved = result.get_type(ctx);
        result.set_type(ctx, fp64);
        assert!(verify_lossless_module(module, ctx, &facts).is_err());
        result.set_type(ctx, saved);
        verify_lossless_module(module, ctx, &facts)?;
    }

    // 3. Degrading the aggregate return layout back to the unspecialized f64-field record must fail.
    let arg_types = TypedHandle::<FunctionType>::from_handle(spec_fn.get_type(ctx), ctx)?
        .deref(ctx)
        .arg_types();
    let degraded_sig = FunctionType::get(ctx, arg_types, vec![orig_ret_ty]);
    spec_fn.set_attr_builtin_func_type(ctx, TypeAttr::new(degraded_sig.into()));
    assert!(verify_lossless_module(module, ctx, &facts).is_err());

    // A pre-existing incompatible layout must report a type error, including recursive reservations.
    let ctx = &mut Context::new();
    let module = parse(ctx, text);
    set_exported(ctx, &function(ctx, module, "run"));
    RecordType::get(ctx, layout_name, Some(Vec::new()))?;
    let result = Specialize.run(module, ctx, &mut AnalysisManager::default());
    assert!(
        matches!(result, Err(error) if error.to_string().contains("already exists with different fields"))
    );
    Ok(())
}
