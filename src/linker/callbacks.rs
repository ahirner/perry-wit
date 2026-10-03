//! Generate calls into the TypeScript table without Rust function-pointer casts.

use anyhow::{Context, Result, ensure};
use wasm_encoder::{BlockType, Function, Instruction, ValType};
use wasmparser::{ElementItems, ElementKind, Operator};

use super::sections::{ParsedModuleA, ParsedModuleB};

const UNDEFINED: i64 = 0x7ffc_0000_0000_0001;

pub(crate) struct CallbackBridge {
    pub(crate) invoke: u32,
    table: u32,
    argument: u32,
    invalid: u32,
}

impl CallbackBridge {
    pub(crate) fn discover(runtime: &ParsedModuleB) -> Result<Option<Self>> {
        let Some(&invoke) = runtime.export_funcs.get("guest_callback_invoke") else {
            return Ok(None);
        };
        let helper = |name| {
            runtime
                .export_funcs
                .get(name)
                .copied()
                .with_context(|| format!("Missing callback ABI helper: {name}"))
        };
        Ok(Some(Self {
            invoke,
            table: helper("guest_callback_table")?,
            argument: helper("guest_callback_argument")?,
            invalid: helper("guest_callback_invalid")?,
        }))
    }

    pub(crate) fn dependencies(&self) -> [u32; 3] {
        [self.table, self.argument, self.invalid]
    }

    pub(crate) fn synthesize(
        &self,
        module: &ParsedModuleA,
        functions: &[u32],
        types: &[u32],
    ) -> Result<Function> {
        let mut function = Function::new([(1, ValType::I32)]);
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::LocalGet(1));
        function.instruction(&Instruction::Call(functions[self.table as usize]));
        function.instruction(&Instruction::LocalSet(2));

        for element in &module.elements {
            let ElementKind::Active {
                table_index: None | Some(0),
                ref offset_expr,
            } = element.kind
            else {
                continue;
            };
            let mut offset_reader = offset_expr.get_operators_reader();
            let Operator::I32Const { value: offset } = offset_reader.read()? else {
                anyhow::bail!("Callback table requires a constant element offset");
            };
            ensure!(
                matches!(offset_reader.read()?, Operator::End) && offset_reader.eof(),
                "Callback table requires a single constant element offset"
            );
            ensure!(offset >= 0, "Callback table offset must be nonnegative");
            let ElementItems::Functions(ref entries) = element.items else {
                continue;
            };
            for (position, entry) in entries.clone().into_iter().enumerate() {
                let entry = entry? as usize;
                let type_index = if entry < module.imports.len() {
                    module.imports[entry].2
                } else {
                    module.func_types[entry - module.imports.len()]
                };
                let signature = &module.types[type_index as usize];
                if signature
                    .params()
                    .iter()
                    .any(|param| *param != wasmparser::ValType::I64)
                    || !matches!(signature.results(), [] | [wasmparser::ValType::I64])
                {
                    continue;
                }
                let table_index = (offset as u32)
                    .checked_add(position as u32)
                    .context("Callback table index overflow")?;
                function.instruction(&Instruction::LocalGet(2));
                function.instruction(&Instruction::I32Const(table_index as i32));
                function.instruction(&Instruction::I32Eq);
                function.instruction(&Instruction::If(BlockType::Empty));
                for index in 0..signature.params().len() {
                    function.instruction(&Instruction::LocalGet(0));
                    function.instruction(&Instruction::LocalGet(1));
                    function.instruction(&Instruction::I32Const(index as i32));
                    function.instruction(&Instruction::Call(functions[self.argument as usize]));
                }
                function.instruction(&Instruction::LocalGet(2));
                function.instruction(&Instruction::CallIndirect {
                    type_index: types[type_index as usize],
                    table_index: 0,
                });
                if signature.results().is_empty() {
                    function.instruction(&Instruction::I64Const(UNDEFINED));
                }
                function.instruction(&Instruction::Return);
                function.instruction(&Instruction::End);
            }
        }
        function.instruction(&Instruction::Call(functions[self.invalid as usize]));
        function.instruction(&Instruction::End);
        Ok(function)
    }
}
