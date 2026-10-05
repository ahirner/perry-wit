//! Test-only assertion that a suspended canonical return area remains allocated.

use anyhow::{Context, Result, ensure};
use waffle::{
    BlockTarget, FuncDecl, FunctionBody, GlobalData, ImportKind, MemoryArg, Module, Operator,
    SignatureData, Terminator, Type, ValueDef,
};

pub fn guard_return_area(
    core: &[u8],
    interface: &str,
    pending: &str,
    release: &str,
) -> Result<Vec<u8>> {
    let mut module = Module::from_wasm_bytes(core, &Default::default())?;
    module.expand_all_funcs()?;
    let import = |name: &str| {
        module
            .imports
            .iter()
            .find_map(|import| match import.kind {
                ImportKind::Func(function) if import.module == interface && import.name == name => {
                    Some(function)
                }
                _ => None,
            })
            .with_context(|| format!("missing probe import {interface}#{name}"))
    };
    let pending = import(pending)?;
    let release = import(release)?;
    ensure!(
        module.signatures[module.funcs[pending].sig()].params == [Type::I32],
        "probe requires a single return pointer"
    );
    let address = module.globals.push(GlobalData {
        ty: Type::I32,
        value: Some(0),
        mutable: true,
    });
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32],
        returns: vec![],
    });
    let mut guard = FunctionBody::new(&module, sig);
    let entry = guard.entry;
    let pointer = guard.blocks[entry].params[0].1;
    let four = guard.add_op(entry, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
    let backlink = guard.add_op(entry, Operator::I32Sub, &[pointer, four], &[Type::I32]);
    let memory = MemoryArg {
        memory: module.memories.iter().next().context("missing memory")?,
        offset: 0,
        align: 2,
    };
    let header = guard.add_op(
        entry,
        Operator::I32Load { memory },
        &[backlink],
        &[Type::I32],
    );
    let kind = guard.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                offset: 16,
                ..memory
            },
        },
        &[header],
        &[Type::I32],
    );
    let free = guard.add_op(
        entry,
        Operator::I32Const { value: u32::MAX },
        &[],
        &[Type::I32],
    );
    let allocated = guard.add_op(entry, Operator::I32Ne, &[kind, free], &[Type::I32]);
    let valid = guard.add_block();
    let invalid = guard.add_block();
    guard.set_terminator(
        entry,
        Terminator::CondBr {
            cond: allocated,
            if_true: BlockTarget {
                block: valid,
                args: vec![],
            },
            if_false: BlockTarget {
                block: invalid,
                args: vec![],
            },
        },
    );
    guard.set_terminator(valid, Terminator::Return { values: vec![] });
    guard.set_terminator(invalid, Terminator::Unreachable);
    guard.validate()?;
    let check = module.funcs.push(FuncDecl::Body(
        sig,
        "test.assert-return-allocated".into(),
        guard,
    ));
    let mut pending_sites = 0;
    let mut release_sites = 0;
    for (_, function) in module.funcs.entries_mut() {
        let FuncDecl::Body(_, _, body) = function else {
            continue;
        };
        let blocks: Vec<_> = body.blocks.iter().collect();
        for block in blocks {
            let instructions = std::mem::take(&mut body.blocks[block].insts);
            for instruction in instructions {
                if let ValueDef::Operator(Operator::Call { function_index }, args, _) =
                    body.values[instruction].clone()
                {
                    if function_index == pending {
                        let pointer = body.arg_pool[args][0];
                        body.add_op(
                            block,
                            Operator::GlobalSet {
                                global_index: address,
                            },
                            &[pointer],
                            &[],
                        );
                        pending_sites += 1;
                    } else if function_index == release {
                        let pointer = body.add_op(
                            block,
                            Operator::GlobalGet {
                                global_index: address,
                            },
                            &[],
                            &[Type::I32],
                        );
                        body.add_op(
                            block,
                            Operator::Call {
                                function_index: check,
                            },
                            &[pointer],
                            &[],
                        );
                        release_sites += 1;
                    }
                }
                body.blocks[block].insts.push(instruction);
            }
        }
        body.validate()?;
    }
    ensure!(
        pending_sites == 1 && release_sites == 1,
        "probe must guard exactly one pending import and one release"
    );
    module.to_wasm_bytes()
}

/// Force collection at ownership handoffs, not only at source loop backedges.
pub fn collect_at_result_handoffs(
    compiled: &perry_wit::waffle_backend::WaffleCompiled,
) -> Result<Vec<u8>> {
    let mut module = Module::from_wasm_bytes(&compiled.core, &Default::default())?;
    module.expand_all_funcs()?;
    // Linking preserves the order of core definitions, then appends helper bodies.
    let names = compiled
        .waffle_ir
        .lines()
        .filter(|line| line.starts_with("  func") && line.contains(" = #"))
        .map(|line| line.split('"').nth(1).context("missing function name"))
        .collect::<Result<Vec<_>>>()?;
    let definitions = module
        .funcs
        .entries_mut()
        .filter_map(|(_, function)| match function {
            FuncDecl::Body(_, name, _) => Some(name),
            _ => None,
        });
    for (name, original) in definitions.zip(names) {
        *name = original.into();
    }
    let collect = module
        .funcs
        .entries()
        .find_map(|(id, function)| match function {
            FuncDecl::Body(_, name, _) if name == "collect" || name == "$collect" => Some(id),
            _ => None,
        })
        .context("missing collector")?;
    // Poison reclaimed payloads so dangling pointers fail even before allocator reuse.
    let poison = wat::parse_str(
        r#"(module
      (import "probe" "collect" (func $collect))
      (memory 1)
      (func (export "poison") (local $block i32)
        (call $collect)
        (local.set $block (i32.load (i32.const 36)))
        (block $done (loop $scan
          (br_if $done (i32.eqz (local.get $block)))
          (if (i32.eq (i32.load offset=16 (local.get $block)) (i32.const -1)) (then
            (memory.fill (i32.add (local.get $block) (i32.const 32)) (i32.const 165)
              (i32.sub (i32.load offset=4 (local.get $block)) (i32.const 32)))))
          (local.set $block (i32.load (local.get $block)))
          (br $scan)))))"#,
    )?;
    let mut poison = Module::from_wasm_bytes(&poison, &Default::default())?;
    poison.expand_all_funcs()?;
    let body = poison
        .funcs
        .entries()
        .find_map(|(_, function)| match function {
            FuncDecl::Body(_, _, body) => Some(body.clone()),
            _ => None,
        })
        .context("missing poison body")?;
    let mut body = body;
    let memory = module.memories.iter().next().context("missing memory")?;
    for (_, value) in body.values.entries_mut() {
        if let ValueDef::Operator(operator, _, _) = value {
            match operator {
                Operator::Call { function_index } => *function_index = collect,
                Operator::I32Load { memory: argument } => argument.memory = memory,
                Operator::MemoryFill { mem } => *mem = memory,
                _ => {}
            }
        }
    }
    let signature = module.funcs[collect].sig();
    let collect = module.funcs.push(FuncDecl::Body(
        signature,
        "probe.poison-collected".into(),
        body,
    ));
    let targets: std::collections::BTreeSet<_> = module
        .funcs
        .entries()
        .filter_map(|(id, function)| match function {
            FuncDecl::Body(_, name, _)
                if name.ends_with(".export")
                    || name.ends_with(".task-return")
                    || name == "tasks.validate"
                    || name == "tasks.finish" =>
            {
                Some(id)
            }
            _ => None,
        })
        .collect();
    let mut sites = 0;
    for (_, function) in module.funcs.entries_mut() {
        let FuncDecl::Body(_, name, body) = function else {
            continue;
        };
        let worker = name.ends_with(".worker");
        let blocks: Vec<_> = body.blocks.iter().collect();
        for block in blocks {
            let instructions = std::mem::take(&mut body.blocks[block].insts);
            for instruction in instructions {
                let target = match &body.values[instruction] {
                    ValueDef::Operator(Operator::Call { function_index }, _, _) => {
                        targets.contains(function_index)
                    }
                    _ => false,
                };
                if target && !worker {
                    body.add_op(
                        block,
                        Operator::Call {
                            function_index: collect,
                        },
                        &[],
                        &[],
                    );
                    sites += 1;
                }
                body.blocks[block].insts.push(instruction);
                if target && worker {
                    body.add_op(
                        block,
                        Operator::Call {
                            function_index: collect,
                        },
                        &[],
                        &[],
                    );
                    sites += 1;
                }
            }
        }
        body.validate()?;
    }
    ensure!(
        sites >= 2,
        "expected worker and publication collection sites, found {sites}"
    );
    module.to_wasm_bytes()
}
