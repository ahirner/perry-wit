use super::*;

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
    let wait = functions
        .iter()
        .find(|f| f.get_symbol_name(ctx).as_ref() == "wait")
        .unwrap();
    crate::calls::set_exported(ctx, wait);
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
                "worker" => Failure::Rejects { escapes: true },
                "launch" => Failure::None,
                _ => Failure::Throws { escapes: true },
            },
            "{name}"
        );
    }
    let wait = effects.effects(&pliron::ident!("wait")).unwrap();
    assert!(wait.effects.may_mutate_records());
    assert_eq!(wait.effects.failure, Failure::Throws { escapes: true });
    let launch = effects.effects(&pliron::ident!("launch")).unwrap();
    assert!(launch.effects.allocates && launch.effects.may_collect && launch.effects.may_suspend);
    assert_eq!(
        effects.effects(&pliron::ident!("rotate")).unwrap().escapes,
        std::collections::BTreeSet::from([0, 1])
    );
    for name in ["left", "right", "worker", "launch", "rotate"] {
        assert!(
            !effects
                .effects(&name.try_into().unwrap())
                .unwrap()
                .effects
                .repeated_allocations,
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn repeated_allocation_depends_on_call_and_allocation_placement() -> Result<()> {
    use crate::facts::ProgramFacts;
    use pliron::pass::AnalysisManager;
    let fixture = r#"
builtin.module @repetition { ^module():
  builtin.func @pure: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.fp64)> {
    ^entry(value: builtin.fp64, recurse: builtin.integer i1):
      perry.cond_br if recurse ^again() else ^done(value)
    ^again():
      no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
      next = perry.call pure (value, no) : builtin.fp64;
      perry.br ^done(next)
    ^done(result: builtin.fp64):
      perry.return result
  };
  builtin.func @allocate: builtin.function <(builtin.fp64) -> (perry.record <Cell { value: builtin.fp64 }>)> {
    ^entry(value: builtin.fp64):
      cell = perry.alloc Unassigned (value) : perry.record <Cell>;
      perry.return cell
  };
  builtin.func @recursive: builtin.function <(builtin.fp64, builtin.integer i1) -> (perry.record <Cell>)> {
    ^entry(value: builtin.fp64, again: builtin.integer i1):
      cell = perry.call allocate (value) : perry.record <Cell>;
      perry.cond_br if again ^recurse() else ^done(cell)
    ^recurse():
      no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
      next = perry.call recursive (value, no) : perry.record <Cell>;
      perry.br ^done(next)
    ^done(result: perry.record <Cell>):
      perry.return result
  };
  builtin.func @run: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.fp64)> {
    ^entry(value: builtin.fp64, again: builtin.integer i1):
      ENTER
      perry.br ^loop()
    ^loop():
      BODY
      perry.cond_br if again ^loop() else ^done()
    ^done():
      EXIT
      perry.return value
  }
}
"#;
    for statement in [
        "cell = perry.alloc Unassigned (value) : perry.record <Cell>;",
        "cell = perry.call allocate (value) : perry.record <Cell>;",
        "result = perry.call pure (value, again) : builtin.fp64;",
        "cell = perry.call recursive (value, again) : perry.record <Cell>;",
    ] {
        for position in ["ENTER", "BODY", "EXIT"] {
            let mut input = fixture.replace(position, statement);
            for unused in ["ENTER", "BODY", "EXIT"] {
                input = input.replace(unused, "");
            }
            let ctx = &mut Context::new();
            let module = parse(ctx, &input);
            verify_operation(module, ctx)?;
            let mut analyses = AnalysisManager::default();
            let facts = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
            assert!(
                !facts
                    .effects(&pliron::ident!("pure"))
                    .unwrap()
                    .effects
                    .repeated_allocations
            );
            assert!(
                !facts
                    .effects(&pliron::ident!("allocate"))
                    .unwrap()
                    .effects
                    .repeated_allocations
            );
            assert!(
                facts
                    .effects(&pliron::ident!("recursive"))
                    .unwrap()
                    .effects
                    .repeated_allocations
            );
            let expected = !statement.contains("call pure")
                && (position == "BODY" || statement.contains("call recursive"));
            assert_eq!(
                facts
                    .effects(&pliron::ident!("run"))
                    .unwrap()
                    .effects
                    .repeated_allocations,
                expected,
                "{position}: {statement}"
            );
        }
    }
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
    let module = parse(ctx, &ASYNC_STEP.replace("perry.return i", "absent = perry.undefined : perry.optional <builtin.fp64,Undefined>; value = perry.unwrap absent : builtin.fp64; perry.return value"));
    let mut analyses = AnalysisManager::default();
    let run = pliron::ident!("run");
    {
        let effects = analyses.get_analysis::<ProgramFacts>(module, ctx)?;
        assert_eq!(
            effects.effects(&run).unwrap().effects.failure,
            Failure::Throws { escapes: true }
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
        Failure::Rejects { escapes: true }
    );
    let fresh = ProgramFacts::compute(module, ctx, &mut AnalysisManager::default())?;
    for function in crate::aggregates::functions(module, ctx) {
        let name = function.get_symbol_name(ctx);
        assert_eq!(effects.effects(&name), fresh.effects(&name));
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
