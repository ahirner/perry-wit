//! Index remapping and const expression re-encoding for module linking.

use anyhow::{Result, bail};
use wasm_encoder::ConstExpr;
use wasm_encoder::Instruction;
use wasm_encoder::reencode::{self, Reencode};
use wasmparser::Operator;

pub(crate) fn reencode_const_expr(
    expr: &wasmparser::ConstExpr<'_>,
    global_map: impl Fn(u32) -> u32,
    func_map: impl Fn(u32) -> u32,
) -> Result<ConstExpr> {
    let mut reader = expr.get_operators_reader();
    let mut instrs = Vec::new();
    while !reader.eof() {
        match reader.read()? {
            Operator::I32Const { value } => instrs.push(Instruction::I32Const(value)),
            Operator::I64Const { value } => instrs.push(Instruction::I64Const(value)),
            Operator::F32Const { value } => {
                instrs.push(Instruction::F32Const(f32::from_bits(value.bits()).into()))
            }
            Operator::F64Const { value } => {
                instrs.push(Instruction::F64Const(f64::from_bits(value.bits()).into()))
            }
            Operator::GlobalGet { global_index } => {
                instrs.push(Instruction::GlobalGet(global_map(global_index)))
            }
            Operator::RefFunc { function_index } => {
                instrs.push(Instruction::RefFunc(func_map(function_index)))
            }
            Operator::RefNull { .. } => {
                instrs.push(Instruction::RefNull(wasm_encoder::HeapType::Abstract {
                    shared: false,
                    ty: wasm_encoder::AbstractHeapType::Func,
                }))
            }
            Operator::End => break,
            other => bail!("Unsupported operator in const expr: {other:?}"),
        }
    }
    if instrs.len() == 1 {
        match instrs[0] {
            Instruction::I32Const(v) => Ok(ConstExpr::i32_const(v)),
            Instruction::I64Const(v) => Ok(ConstExpr::i64_const(v)),
            Instruction::GlobalGet(g) => Ok(ConstExpr::global_get(g)),
            Instruction::RefFunc(f) => Ok(ConstExpr::ref_func(f)),
            _ => Ok(ConstExpr::extended(instrs)),
        }
    } else {
        Ok(ConstExpr::extended(instrs))
    }
}

pub(crate) struct ReencodeA<'a> {
    pub(crate) func_map: &'a [u32],
    pub(crate) type_map: &'a [u32],
}

impl Reencode for ReencodeA<'_> {
    type Error = String;

    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        self.func_map.get(index as usize).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("Module A function index out of range: {index}"))
        })
    }

    fn type_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        self.type_map.get(index as usize).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("Module A type index out of range: {index}"))
        })
    }

    fn table_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        if index == 0 {
            Ok(0)
        } else {
            Err(reencode::Error::UserError(format!(
                "Module A unexpected table index: {index}"
            )))
        }
    }

    fn global_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        Ok(index)
    }

    fn memory_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        if index == 0 {
            Ok(0)
        } else {
            Err(reencode::Error::UserError(format!(
                "Module A unexpected memory index: {index}"
            )))
        }
    }

    fn instruction<'a>(
        &mut self,
        operator: Operator<'a>,
    ) -> Result<Instruction<'a>, reencode::Error<Self::Error>> {
        reencode::utils::instruction(self, operator)
    }
}

pub(crate) struct ReencodeB<'a> {
    pub(crate) func_map: &'a [u32],
    pub(crate) type_map: &'a [u32],
    pub(crate) global_offset: u32,
}

impl Reencode for ReencodeB<'_> {
    type Error = String;

    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        self.func_map.get(index as usize).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("Module B function index out of range: {index}"))
        })
    }

    fn type_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        self.type_map.get(index as usize).copied().ok_or_else(|| {
            reencode::Error::UserError(format!("Module B type index out of range: {index}"))
        })
    }

    fn table_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        // Module B's table becomes Table 1
        Ok(index + 1)
    }

    fn global_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        Ok(index + self.global_offset)
    }

    fn memory_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        if index == 0 {
            Ok(0)
        } else {
            Err(reencode::Error::UserError(format!(
                "Module B unexpected memory index: {index}"
            )))
        }
    }

    fn instruction<'a>(
        &mut self,
        operator: Operator<'a>,
    ) -> Result<Instruction<'a>, reencode::Error<Self::Error>> {
        reencode::utils::instruction(self, operator)
    }
}
