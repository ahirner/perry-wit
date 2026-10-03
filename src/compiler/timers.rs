//! Direct timer bridges keep polling dependencies out of unrelated capability dispatch.

use std::collections::BTreeSet;
use std::convert::Infallible;

use anyhow::{Context, Result};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{CodeSection, EntityType, Function, ImportSection, Instruction, Module};
use wasmparser::{FunctionBody, ImportSectionReader, Operator, Parser};

use super::exceptions::{memory_call_name, runtime_string_names};
use crate::linker::sections::parse_module_a;

/// Adds one import and remaps every function reference when timer calls are present.
pub(super) fn specialize_timer_calls(bytes: &[u8]) -> Result<Vec<u8>> {
    let parsed = parse_module_a(bytes)?;
    let import = |name| {
        parsed
            .imports
            .iter()
            .position(|(module, field, _)| *module == "rt" && *field == name)
            .with_context(|| format!("missing runtime import {name}"))
    };
    let mem_call = import("mem_call")?;
    let string_new = import("string_new")?;
    let operators = parsed
        .bodies
        .iter()
        .map(|body| {
            body.get_operators_reader()?
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let names = runtime_string_names(&operators, &parsed.data, string_new as u32)?;
    let calls: Vec<BTreeSet<usize>> = operators.iter().map(|operators| {
        operators.iter().enumerate().filter_map(|(index, operator)| {
            (matches!(operator, Operator::Call { function_index } if *function_index == mem_call as u32)
                && matches!(memory_call_name(operators, index, &names), Some(("timer_schedule" | "timer_cancel", _))))
                .then_some(index)
        }).collect()
    }).collect();
    if calls.iter().all(BTreeSet::is_empty) {
        return Ok(bytes.to_vec());
    }
    let mut reencoder = TimerCalls {
        import_count: parsed.imports.len() as u32,
        timer_type: parsed.imports[mem_call].2,
        calls,
        next_body: 0,
    };
    let mut module = Module::new();
    reencoder
        .parse_core_module(&mut module, Parser::new(0), bytes)
        .map_err(|error| anyhow::anyhow!("specializing timer calls: {error}"))?;
    Ok(module.finish())
}

/// Maintains module indices while only replacing the selected call instructions.
struct TimerCalls {
    import_count: u32,
    timer_type: u32,
    calls: Vec<BTreeSet<usize>>,
    next_body: usize,
}

impl Reencode for TimerCalls {
    type Error = Infallible;

    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        Ok(index + u32::from(index >= self.import_count))
    }

    fn parse_import_section(
        &mut self,
        imports: &mut ImportSection,
        reader: ImportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_import_section(self, imports, reader)?;
        imports.import(
            "rt",
            "mem_call_timers",
            EntityType::Function(self.timer_type),
        );
        Ok(())
    }

    fn parse_function_body(
        &mut self,
        code: &mut CodeSection,
        body: FunctionBody<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        let mut function = Function::new(
            body.get_locals_reader()?
                .into_iter()
                .map(|local| {
                    let (count, ty) = local?;
                    Ok((count, self.val_type(ty)?))
                })
                .collect::<Result<Vec<_>, reencode::Error<Self::Error>>>()?,
        );
        for (index, operator) in body.get_operators_reader()?.into_iter().enumerate() {
            let instruction = if self.calls[self.next_body].contains(&index) {
                Instruction::Call(self.import_count)
            } else {
                self.instruction(operator?)?
            };
            function.instruction(&instruction);
        }
        code.function(&function);
        self.next_body += 1;
        Ok(())
    }
}
