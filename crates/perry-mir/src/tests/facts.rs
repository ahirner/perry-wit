use super::*;

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
fn aggregate_facts_follow_calls_elements_and_aliased_writes() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
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
            let run = crate::aggregates::functions(module, ctx)
                .into_iter()
                .find(|f| f.get_symbol_name(ctx) == pliron::ident!("run"))
                .unwrap();
            crate::calls::set_exported(ctx, &run);
            let mut analyses = AnalysisManager::default();
            let facts = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
            let returned = crate::aggregates::operations(ctx, &run)
                .into_iter()
                .find(|&op| Operation::get_op::<crate::control::ReturnOp>(op, ctx).is_some())
                .unwrap();
            for (value, expected) in returned.deref(ctx).operands().zip(expected) {
                assert_eq!(facts.is_count(ctx, &run, value), expected, "{mutation}");
            }
        }
    }
    Ok(())
}

#[test]
fn array_lengths_follow_aliases_calls_and_growth() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::{
        builtin::op_interfaces::SymbolOpInterface,
        pass::{Analysis, AnalysisManager},
    };
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
            use pliron::op::Op;
            let write = crate::aggregates::functions(module, ctx)
                .into_iter()
                .find(|f| f.get_symbol_name(ctx) == pliron::ident!("write"))
                .unwrap();
            let entry = write.get_entry_block(ctx);
            let alias = entry.deref(ctx).get_argument(0);
            let end = entry.deref(ctx).get_terminator(ctx).unwrap();
            crate::collections::ArrayExtendOp::new(ctx, alias, alias)
                .get_operation()
                .insert_before(ctx, end);
        }
        verify_operation(module, ctx)?;
        let run = crate::aggregates::functions(module, ctx)
            .into_iter()
            .find(|f| f.get_symbol_name(ctx) == pliron::ident!("run"))
            .unwrap();
        crate::calls::set_exported(ctx, &run);
        let read = crate::aggregates::operations(ctx, &run)
            .into_iter()
            .find(|&op| Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some())
            .unwrap();
        let array = read.deref(ctx).get_operand(0);
        let index = read.deref(ctx).get_operand(1);
        let mut analyses = AnalysisManager::default();
        {
            let cached = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
            let fresh = ProgramFacts::compute(module, ctx, &mut AnalysisManager::default())?;
            for facts in [&*cached, &fresh] {
                assert_eq!(
                    facts.array_length(ctx, &run, array),
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
        crate::pipeline::canonicalize(module, ctx)?;
        let operations = crate::aggregates::operations(ctx, &run);
        let checked = operations
            .iter()
            .any(|&op| Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some());
        let length = operations
            .iter()
            .any(|&op| Operation::get_op::<crate::collections::ArrayLenOp>(op, ctx).is_some());
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
fn promise_outcomes_follow_aggregate_aliases_and_recursive_calls() -> Result<()> {
    use crate::{effects::Failure, facts::ProgramFacts};
    use pliron::{
        builtin::op_interfaces::SymbolOpInterface,
        pass::{Analysis, AnalysisManager},
    };
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
            let run = crate::aggregates::functions(root, ctx)
                .into_iter()
                .find(|f| f.get_symbol_name(ctx) == pliron::ident!("run"))
                .unwrap();
            crate::calls::set_async(ctx, &run);
            crate::calls::set_exported(ctx, &run);
            verify_operation(root, ctx)?;
            let pending = crate::aggregates::operations(ctx, &run)
                .into_iter()
                .find(|&op| Operation::get_op::<crate::calls::AwaitOp>(op, ctx).is_some())
                .unwrap()
                .deref(ctx)
                .get_operand(0);
            let mut analyses = AnalysisManager::default();
            let cached = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
            let fresh = ProgramFacts::compute(root, ctx, &mut AnalysisManager::default())?;
            for facts in [&*cached, &fresh] {
                assert_eq!(facts.may_reject(ctx, &run, pending), rejects, "{mutation}");
                let expected = if rejects {
                    Failure::Rejects { escapes: !handled }
                } else {
                    Failure::None
                };
                assert_eq!(
                    facts
                        .effects(&pliron::ident!("run"))
                        .unwrap()
                        .effects
                        .failure,
                    expected,
                    "{mutation}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn combinator_outcomes_join_inputs_without_mutating_them() -> Result<()> {
    use crate::{
        facts::ProgramFacts,
        intrinsics::{Combinator, PromiseCombinatorOp},
    };
    use pliron::{builtin::op_interfaces::OneResultInterface, op::Op, pass::AnalysisManager};
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
            let run = crate::aggregates::functions(root, ctx)[0];
            crate::calls::set_async(ctx, &run);
            crate::calls::set_exported(ctx, &run);
            let ops = crate::aggregates::operations(ctx, &run);
            let array = ops
                .iter()
                .find_map(|&op| Operation::get_op::<crate::collections::ArrayNewOp>(op, ctx))
                .unwrap()
                .get_result(ctx);
            let end = *ops.last().unwrap();
            let group = PromiseCombinatorOp::new(ctx, kind, array).unwrap();
            group.get_operation().insert_before(ctx, end);
            let pending = group.get_result(ctx);
            let wait = crate::calls::AwaitOp::new(ctx, pending)?;
            wait.get_operation().insert_before(ctx, end);
            verify_operation(root, ctx)?;
            let mut analyses = AnalysisManager::default();
            let facts = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
            assert_eq!(
                facts.may_reject(ctx, &run, pending),
                rejected && kind != Combinator::AllSettled
            );
            assert_eq!(facts.array_length(ctx, &run, array), Some((1, 1)));
        }
    }
    Ok(())
}

#[test]
fn aggregate_summary_bounds_preserve_existing_alias_soundness() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::{builtin::op_interfaces::SymbolOpInterface, pass::AnalysisManager};
    use std::fmt::Write;
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
        let run = crate::aggregates::functions(module, ctx)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(run.get_symbol_name(ctx), pliron::ident!("run"));
        let mut analyses = AnalysisManager::default();
        let facts = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
        let array = crate::aggregates::operations(ctx, &run)
            .into_iter()
            .find(|&op| Operation::get_op::<crate::collections::ArrayNewOp>(op, ctx).is_some())
            .unwrap()
            .deref(ctx)
            .get_result(0);
        assert_eq!(facts.array_length(ctx, &run, array), Some((1, u32::MAX)));
        let returned = run
            .get_entry_block(ctx)
            .deref(ctx)
            .get_terminator(ctx)
            .unwrap()
            .deref(ctx)
            .get_operand(0);
        assert_eq!(facts.is_count(ctx, &run, returned), size <= 32);
        assert_eq!(facts.summarized_heap_sets > 0, size > 32);
        visits.push(facts.constraint_visits);
    }
    for pair in visits.windows(2) {
        assert!(pair[1] < 5 * pair[0], "{visits:?}");
    }
    Ok(())
}

#[test]
fn context_summarization_bounds_mixed_integer_and_float_arguments() -> Result<()> {
    use crate::facts::{ProgramFacts, with_fact_limits};
    use pliron::{
        builtin::{
            op_interfaces::SymbolOpInterface,
            types::{FP64Type, IntegerType, Signedness},
        },
        pass::AnalysisManager,
    };
    use std::fmt::Write;
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
    for func in crate::aggregates::functions(module, ctx) {
        let sym = func.get_symbol_name(ctx);
        if let Some(rest) = sym.as_ref().strip_prefix('e')
            && let Ok(mask) = rest.parse::<usize>()
        {
            crate::calls::set_exported(ctx, &func);
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
            crate::facts::constrain_signature(ctx, &func, args, results);
        }
    }
    let mut analyses = AnalysisManager::default();
    let facts = with_fact_limits(2, 2, || analyses.get_analysis::<ProgramFacts>(module, ctx))?;
    // One generic context, 2 precise ones and one shared summary.
    assert_eq!(facts.context_counts(&pliron::ident!("helper")), (1, 3));
    assert_eq!(facts.summarized_contexts, combos - 2);
    Ok(())
}

#[test]
fn lossless_representation_verifier_rejects_precision_degradation() -> Result<()> {
    use crate::facts::{ProgramFacts, Specialize, verify_lossless_module};
    use pliron::{
        builtin::{
            attributes::TypeAttr,
            op_interfaces::SymbolOpInterface,
            type_interfaces::FunctionTypeInterface,
            types::{FP64Type, FunctionType},
        },
        pass::{AnalysisManager, Pass},
        r#type::{TypeHandle, Typed, TypedHandle},
    };
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
      perry.return copy
  }
}
"#;
    let module = parse(ctx, text);
    let run = crate::aggregates::functions(module, ctx)[0];
    crate::calls::set_exported(ctx, &run);
    let orig_ret_ty = TypedHandle::<FunctionType>::from_handle(run.get_type(ctx), ctx)?
        .deref(ctx)
        .res_types()[0];
    verify_operation(module, ctx)?;

    let mut analyses = AnalysisManager::default();
    Specialize.run(module, ctx, &mut analyses)?;
    verify_operation(module, ctx)?;
    let facts = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
    verify_lossless_module(module, ctx, &facts)?;

    let spec_fn = crate::aggregates::functions(module, ctx)
        .into_iter()
        .find(|f| f.get_symbol_name(ctx) == pliron::ident!("run"))
        .unwrap();
    let fp64: TypeHandle = FP64Type::get(ctx).into();

    // 1. Degrading a ParseInt SSA result back to f64 must fail lossless verification.
    let parse_res = crate::aggregates::operations(ctx, &spec_fn)
        .into_iter()
        .find(|&op| Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx).is_some())
        .unwrap()
        .deref(ctx)
        .get_result(0);
    let saved_parse_ty = parse_res.get_type(ctx);
    parse_res.set_type(ctx, fp64);
    assert!(verify_lossless_module(module, ctx, &facts).is_err());
    parse_res.set_type(ctx, saved_parse_ty);
    verify_lossless_module(module, ctx, &facts)?;

    // 2. Degrading a FieldGetOp of a Fact::Numeric field back to f64 must fail verification.
    let fg_res = crate::aggregates::operations(ctx, &spec_fn)
        .into_iter()
        .find(|&op| Operation::get_op::<crate::heap::FieldGetOp>(op, ctx).is_some())
        .unwrap()
        .deref(ctx)
        .get_result(0);
    let saved_fg_ty = fg_res.get_type(ctx);
    fg_res.set_type(ctx, fp64);
    assert!(verify_lossless_module(module, ctx, &facts).is_err());
    fg_res.set_type(ctx, saved_fg_ty);
    verify_lossless_module(module, ctx, &facts)?;

    // 3. Degrading the aggregate return layout back to the unspecialized f64-field record must fail.
    let arg_types = TypedHandle::<FunctionType>::from_handle(spec_fn.get_type(ctx), ctx)?
        .deref(ctx)
        .arg_types();
    let degraded_sig = FunctionType::get(ctx, arg_types, vec![orig_ret_ty]);
    spec_fn.set_attr_builtin_func_type(ctx, TypeAttr::new(degraded_sig.into()));
    assert!(verify_lossless_module(module, ctx, &facts).is_err());
    Ok(())
}
