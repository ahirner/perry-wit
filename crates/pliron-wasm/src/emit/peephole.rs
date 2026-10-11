//! Local rewrites of a function's emitted instruction sequence.
use wasm_encoder::{BlockType, Instruction};

/// `local.set n; local.get n` is `local.tee n`, an empty `else` arm goes, and an `if` with no
/// arms only drops its condition.
pub(super) fn peephole(code: Vec<Instruction<'static>>) -> Vec<Instruction<'static>> {
    let mut out: Vec<Instruction<'static>> = Vec::with_capacity(code.len());
    for instruction in code {
        match (&instruction, out.last()) {
            (Instruction::LocalGet(get), Some(Instruction::LocalSet(set))) if get == set => {
                let local = *get;
                out.pop();
                out.push(Instruction::LocalTee(local));
            }
            (Instruction::End, Some(Instruction::Else)) => {
                out.pop();
                if matches!(out.last(), Some(Instruction::If(BlockType::Empty))) {
                    out.pop();
                    out.push(Instruction::Drop);
                } else {
                    out.push(Instruction::End);
                }
            }
            (Instruction::End, Some(Instruction::If(BlockType::Empty))) => {
                out.pop();
                out.push(Instruction::Drop);
            }
            _ => out.push(instruction),
        }
    }
    out
}
