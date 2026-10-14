use super::*;

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
