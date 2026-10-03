//! Adds guest exception branches missing from Perry's memory-call backend.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{BlockType, CodeSection, Function, Instruction, Module, RawSection, ValType};
use wasmparser::{
    Data, DataKind, FuncType, FunctionBody, Operator, Parser, ValType as ParsedValType,
};

use crate::linker::sections::parse_module_a;

const UNDEFINED: i64 = 0x7ffc_0000_0000_0001;

/// Restores bridge frames and unwinds to a catch region or the caller on failure.
pub(super) fn lower_runtime_exceptions(bytes: &[u8]) -> Result<Vec<u8>> {
    let parsed = parse_module_a(bytes)?;
    let import_index = |name| {
        parsed
            .imports
            .iter()
            .position(|(module, imported, _)| *module == "rt" && *imported == name)
            .map(|index| index as u32)
            .with_context(|| format!("missing runtime import {name}"))
    };
    let string_new = import_index("string_new")?;
    let mem_call = import_index("mem_call")?;
    let has_exception = import_index("has_exception")?;
    let get_exception = import_index("get_exception")?;
    let throw_value = import_index("throw_value")?;
    let bodies = parsed
        .bodies
        .iter()
        .map(|body| {
            body.get_operators_reader()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let names = runtime_string_names(&bodies, &parsed.data, string_new)?;
    let events = bodies
        .iter()
        .map(|body| bridge_events(body, mem_call, &names))
        .collect::<Result<Vec<_>>>()?;
    if !events.iter().any(|events| {
        events
            .values()
            .any(|event| event.kind == BridgeKind::Throwing)
    }) {
        return Ok(bytes.to_vec());
    }
    let stack_global = events
        .iter()
        .flat_map(|events| events.values())
        .next()
        .context("missing bridge stack")?
        .stack_global;
    let runtime = ExceptionRuntime {
        has_exception,
        get_exception,
        throw_value,
        stack_global,
    };
    let mut code = CodeSection::new();
    for (index, (operators, events)) in bodies.iter().zip(&events).enumerate() {
        code.function(&lower_function(
            &parsed.bodies[index],
            &parsed.types[parsed.func_types[index] as usize],
            operators,
            events,
            parsed.imports.len(),
            &runtime,
        )?);
    }
    let mut module = Module::new();
    for payload in Parser::new(0).parse_all(bytes) {
        if let Some((id, range)) = payload?.as_section() {
            if id == 10 {
                module.section(&code);
            } else {
                module.section(&RawSection {
                    id,
                    data: &bytes[range.start as usize..range.end as usize],
                });
            }
        }
    }
    Ok(module.finish())
}

/// Resolves the string IDs used by Perry's statically named bridge calls.
pub(super) fn runtime_string_names<'a>(
    bodies: &[Vec<Operator<'_>>],
    data: &[Data<'a>],
    string_new: u32,
) -> Result<Vec<&'a str>> {
    let mut names = Vec::new();
    for window in bodies.iter().flat_map(|body| body.windows(3)) {
        if let [
            Operator::I32Const { value: offset },
            Operator::I32Const { value: length },
            Operator::Call { function_index },
        ] = window
            && *function_index == string_new
        {
            let mut name = None;
            for segment in data {
                if let DataKind::Active { offset_expr, .. } = &segment.kind
                    && let Operator::I32Const { value: base } =
                        offset_expr.get_operators_reader().read()?
                    && let Some(relative) = offset
                        .checked_sub(base)
                        .and_then(|offset| usize::try_from(offset).ok())
                    && let Some(data) = segment.data.get(relative..relative + *length as usize)
                {
                    name = Some(std::str::from_utf8(data)?);
                    break;
                }
            }
            names.push(name.context("missing runtime string data")?);
        }
    }
    Ok(names)
}

struct ExceptionRuntime {
    has_exception: u32,
    get_exception: u32,
    throw_value: u32,
    stack_global: u32,
}

/// Inserts exception regions while preserving existing branch targets and bridge frames.
fn lower_function(
    body: &FunctionBody<'_>,
    signature: &FuncType,
    operators: &[Operator<'_>],
    events: &BTreeMap<usize, BridgeEvent>,
    import_count: usize,
    runtime: &ExceptionRuntime,
) -> Result<Function> {
    let ExceptionRuntime {
        has_exception,
        get_exception,
        throw_value,
        stack_global,
    } = *runtime;
    ensure!(
        matches!(signature.results(), [] | [ParsedValType::I64]),
        "unsupported guest exception return type"
    );
    let mut locals = body
        .get_locals_reader()?
        .into_iter()
        .map(|local| {
            let (count, ty) = local?;
            Ok((count, RoundtripReencoder.val_type(ty)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let saved_stack =
        signature.params().len() as u32 + locals.iter().map(|(count, _)| count).sum::<u32>();
    let block_count = events
        .values()
        .filter(|event| matches!(event.kind, BridgeKind::TryStart | BridgeKind::FinallyStart))
        .count() as u32;
    let finally_count = events
        .values()
        .filter(|event| event.kind == BridgeKind::FinallyStart)
        .count() as u32;
    locals.push((1 + block_count, ValType::I32));
    locals.push((finally_count, ValType::I64));
    let mut function = Function::new(locals);
    function.instruction(&Instruction::GlobalGet(stack_global));
    function.instruction(&Instruction::LocalSet(saved_stack));
    let mut labels = vec![Label::Original];
    let mut next_try_local = saved_stack + 1;
    let mut next_exception_local = saved_stack + 1 + block_count;
    for (offset, operator) in operators.iter().enumerate() {
        let instruction = match operator {
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                labels.push(Label::Original);
                RoundtripReencoder.instruction(operator.clone())?
            }
            Operator::End => {
                ensure!(
                    matches!(labels.pop(), Some(Label::Original)),
                    "unbalanced guest exception region"
                );
                Instruction::End
            }
            Operator::Br { relative_depth } => {
                Instruction::Br(original_depth(&labels, *relative_depth)?)
            }
            Operator::BrIf { relative_depth } => {
                Instruction::BrIf(original_depth(&labels, *relative_depth)?)
            }
            Operator::BrTable { targets } => Instruction::BrTable(
                Cow::Owned(
                    targets
                        .targets()
                        .map(|target| original_depth(&labels, target?))
                        .collect::<Result<Vec<_>>>()?,
                ),
                original_depth(&labels, targets.default())?,
            ),
            _ => RoundtripReencoder.instruction(operator.clone())?,
        };
        function.instruction(&instruction);
        if let Some(event) = events.get(&offset) {
            match event.kind {
                BridgeKind::TryStart | BridgeKind::FinallyStart => {
                    function.instruction(&Instruction::GlobalGet(stack_global));
                    function.instruction(&Instruction::LocalSet(next_try_local));
                    let label = if event.kind == BridgeKind::FinallyStart {
                        function.instruction(&Instruction::Call(get_exception));
                        function.instruction(&Instruction::LocalSet(next_exception_local));
                        let label = Label::Finally {
                            stack: next_try_local,
                            exception: next_exception_local,
                        };
                        next_exception_local += 1;
                        label
                    } else {
                        Label::Catch(next_try_local)
                    };
                    function.instruction(&Instruction::Block(BlockType::Empty));
                    labels.push(label);
                    next_try_local += 1;
                }
                BridgeKind::TryEnd => {
                    let Some(Label::Catch(saved)) = labels.pop() else {
                        bail!("unbalanced guest try_end");
                    };
                    function.instruction(&Instruction::End);
                    function.instruction(&Instruction::LocalGet(saved));
                    function.instruction(&Instruction::GlobalSet(stack_global));
                }
                BridgeKind::FinallyEnd => {
                    let Some(Label::Finally { stack, exception }) = labels.pop() else {
                        bail!("unbalanced guest finally");
                    };
                    function.instruction(&Instruction::End);
                    function.instruction(&Instruction::LocalGet(stack));
                    function.instruction(&Instruction::GlobalSet(stack_global));
                    function.instruction(&Instruction::Call(has_exception));
                    function.instruction(&Instruction::I32Eqz);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    function.instruction(&Instruction::LocalGet(exception));
                    function.instruction(&Instruction::I64Const(UNDEFINED));
                    function.instruction(&Instruction::I64Ne);
                    function.instruction(&Instruction::If(BlockType::Empty));
                    function.instruction(&Instruction::LocalGet(exception));
                    function.instruction(&Instruction::Call(throw_value));
                    function.instruction(&Instruction::End);
                    function.instruction(&Instruction::End);
                }
                BridgeKind::Throwing => emit_exception_branch(
                    &mut function,
                    &labels,
                    has_exception,
                    stack_global,
                    saved_stack,
                    !signature.results().is_empty(),
                ),
            }
        } else if matches!(operator, Operator::Call { function_index } if *function_index as usize >= import_count)
            || matches!(operator, Operator::CallIndirect { .. })
        {
            emit_exception_branch(
                &mut function,
                &labels,
                has_exception,
                stack_global,
                saved_stack,
                !signature.results().is_empty(),
            );
        }
    }
    Ok(function)
}

/// Marks protocol calls once their bridge argument frame has been restored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BridgeKind {
    TryStart,
    TryEnd,
    FinallyStart,
    FinallyEnd,
    Throwing,
}

/// Carries the stack global used by one emitted bridge call.
struct BridgeEvent {
    kind: BridgeKind,
    stack_global: u32,
}

/// Distinguishes existing Wasm branch targets from inserted catch regions.
enum Label {
    Original,
    Catch(u32),
    Finally { stack: u32, exception: u32 },
}

/// Recognizes the pinned backend's memory-call protocol; rejects incompatible layouts.
fn bridge_events(
    operators: &[Operator<'_>],
    mem_call: u32,
    names: &[&str],
) -> Result<BTreeMap<usize, BridgeEvent>> {
    let mut events = BTreeMap::new();
    for (index, operator) in operators.iter().enumerate().skip(5) {
        if !matches!(operator, Operator::Call { function_index } if *function_index == mem_call) {
            continue;
        }
        let Some((name, global_index)) = memory_call_name(operators, index, names) else {
            continue;
        };
        let (kind, suffix_length) = match name {
            "try_start" => (BridgeKind::TryStart, 5),
            "try_end" => (BridgeKind::TryEnd, 5),
            "throw_value" => (BridgeKind::Throwing, 5),
            "date_to_iso_string"
            | "timer_schedule"
            | "json_stringify"
            | "closure_new"
            | "closure_set_capture"
            | "closure_call_0"
            | "closure_call_1"
            | "closure_call_2"
            | "closure_call_3"
            | "closure_call_spread"
            | "uint8array_new"
            | "buffer_alloc"
            | "$$cryptoFillRandom"
            | "fs_read_file_sync"
            | "fs_read_file_binary"
            | "fs_write_file_sync"
            | "fs_mkdir_sync"
            | "fs_readdir_sync"
            | "fs_stat_sync"
            | "fs_unlink_sync"
            | "fs_rmdir_sync" => (BridgeKind::Throwing, 9),
            "__perry_catch_start" => (BridgeKind::TryStart, 10),
            "__perry_catch_end" => (BridgeKind::TryEnd, 10),
            "__perry_finally_start" => (BridgeKind::FinallyStart, 10),
            "__perry_finally_end" => (BridgeKind::FinallyEnd, 10),
            "__perry_exception_resume" => (BridgeKind::Throwing, 10),
            _ => continue,
        };
        let restoration = if suffix_length == 10 {
            ensure!(
                matches!(operators.get(index + suffix_length), Some(Operator::Drop)),
                "unsupported exception marker result"
            );
            index + suffix_length - 1
        } else {
            index + suffix_length
        };
        ensure!(
            matches!(operators.get(restoration), Some(Operator::GlobalSet { global_index: restored }) if *restored == global_index),
            "unsupported Perry bridge frame restoration for {name:?}"
        );
        events.insert(
            index + suffix_length,
            BridgeEvent {
                kind,
                stack_global: global_index,
            },
        );
    }
    Ok(events)
}

/// Identifies statically named memory calls without interpreting their argument expressions.
pub(super) fn memory_call_name<'a>(
    operators: &[Operator<'_>],
    index: usize,
    names: &[&'a str],
) -> Option<(&'a str, u32)> {
    let [
        Operator::F64Const { value: name },
        Operator::F64Const { .. },
        Operator::GlobalGet { global_index },
        Operator::I32Const { .. },
        Operator::I32Sub,
    ] = operators.get(index.checked_sub(5)?..index)?
    else {
        return None;
    };
    Some((
        names.get(f64::from_bits(name.bits()) as usize).copied()?,
        *global_index,
    ))
}

/// Maps existing branches across inserted exception blocks.
fn original_depth(labels: &[Label], original: u32) -> Result<u32> {
    labels
        .iter()
        .rev()
        .enumerate()
        .filter(|(_, label)| matches!(label, Label::Original))
        .nth(original as usize)
        .map(|(depth, _)| depth as u32)
        .context("invalid guest branch depth")
}

/// Branches with the existing value stack intact on success, unwinding it on failure.
fn emit_exception_branch(
    function: &mut Function,
    labels: &[Label],
    has_exception: u32,
    stack_global: u32,
    saved_stack: u32,
    returns_value: bool,
) {
    function.instruction(&Instruction::Call(has_exception));
    function.instruction(&Instruction::If(BlockType::Empty));
    if let Some(depth) = labels
        .iter()
        .rev()
        .position(|label| !matches!(label, Label::Original))
    {
        function.instruction(&Instruction::Br(depth as u32 + 1));
    } else {
        function.instruction(&Instruction::LocalGet(saved_stack));
        function.instruction(&Instruction::GlobalSet(stack_global));
        if returns_value {
            function.instruction(&Instruction::I64Const(UNDEFINED));
        }
        function.instruction(&Instruction::Return);
    }
    function.instruction(&Instruction::End);
}
