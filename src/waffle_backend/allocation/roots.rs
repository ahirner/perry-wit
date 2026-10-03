//! Root frames follow source activations, including native suspended stacks.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use waffle::{Block, FunctionBody, Memory, MemoryArg, Operator, Terminator, Type, Value};

use super::AllocationFuncs;

/// Each reference definition owns a slot, overwritten when that definition runs
/// again. This retains operands and encoded return values across calls/finally
/// without keeping an allocation history. Collection is restricted to source
/// backedges; computation helpers do not expose their temporary pointers as roots.
pub(crate) fn track_roots(
    body: &mut FunctionBody,
    memory: Memory,
    allocator: AllocationFuncs,
    references: &BTreeSet<Value>,
    collection_blocks: &BTreeSet<Block>,
) -> Result<()> {
    let slots: BTreeMap<_, _> = references
        .iter()
        .enumerate()
        .map(|(i, &value)| (value, i as u32))
        .collect();
    let entry = body.entry;
    let entry_instructions = std::mem::take(&mut body.blocks[entry].insts);
    let frame = if slots.is_empty() {
        None
    } else {
        let count = body.add_op(
            entry,
            Operator::I32Const {
                value: slots.len() as u32,
            },
            &[],
            &[Type::I32],
        );
        Some(body.add_op(
            entry,
            Operator::Call {
                function_index: allocator.frame_new,
            },
            &[count],
            &[Type::I32],
        ))
    };
    let prologue = std::mem::replace(&mut body.blocks[entry].insts, entry_instructions);
    let blocks: Vec<_> = body.blocks.iter().collect();
    for block in blocks {
        let instructions = std::mem::take(&mut body.blocks[block].insts);
        if block == entry {
            body.blocks[block].insts.extend(prologue.iter().copied());
        }
        let params: Vec<_> = body.blocks[block]
            .params
            .iter()
            .map(|&(_, value)| value)
            .collect();
        for value in params {
            if let Some(&slot) = slots.get(&value) {
                store_root(body, block, memory, frame.unwrap(), slot, value);
            }
        }
        for value in instructions {
            body.blocks[block].insts.push(value);
            if let Some(&slot) = slots.get(&value) {
                store_root(body, block, memory, frame.unwrap(), slot, value);
            }
        }
        if collection_blocks.contains(&block) {
            body.add_op(
                block,
                Operator::Call {
                    function_index: allocator.collect,
                },
                &[],
                &[],
            );
        }
        if matches!(body.blocks[block].terminator, Terminator::Return { .. })
            && let Some(frame) = frame
        {
            body.add_op(
                block,
                Operator::Call {
                    function_index: allocator.frame_drop,
                },
                &[frame],
                &[],
            );
        }
    }
    body.validate()?;
    body.verify_reducible()?;
    Ok(())
}

fn store_root(
    body: &mut FunctionBody,
    block: Block,
    memory: Memory,
    frame: Value,
    slot: u32,
    value: Value,
) {
    body.add_op(
        block,
        Operator::I32Store {
            memory: MemoryArg {
                align: 2,
                offset: 12 + 4 * slot,
                memory,
            },
        },
        &[frame, value],
        &[],
    );
}
