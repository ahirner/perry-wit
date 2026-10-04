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
