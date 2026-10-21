//! Attributes of the `wasm` dialect: numeric opcodes and memory access kinds.
use pliron::derive::pliron_attr;
use wasm_encoder::{Instruction, MemArg, ValType};

use ValType::{F32, F64, I32, I64};

/// How an opcode may be moved by the emitter's stack scheduler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Effect {
    /// No side effects and cannot trap: may be evaluated anywhere its operands are available.
    Pure,
    /// Can trap (integer division by zero, out-of-range truncation): keeps its order relative
    /// to other effects.
    Traps,
}

macro_rules! numeric {
    ($($name:ident: [$($operand:ident),*] -> $result:ident, $effect:ident;)*) => {
        /// A Wasm numeric instruction. The name is the instruction's mnemonic in CamelCase.
        ///
        /// Arithmetic instructions cover `i32`, `i64`, and `f64`, with `f32` conversions.
        #[pliron_attr(name = "wasm.num_opcode", format, verifier = "succ")]
        #[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
        pub enum NumOpcode {
            $($name,)*
        }

        impl NumOpcode {
            /// Operand types and result type.
            pub(crate) fn signature(self) -> (&'static [ValType], ValType) {
                match self {
                    $(NumOpcode::$name => (&[$($operand),*], $result),)*
                }
            }

            pub(crate) fn effect(self) -> Effect {
                match self {
                    $(NumOpcode::$name => Effect::$effect,)*
                }
            }

            pub(crate) fn instruction(self) -> Instruction<'static> {
                match self {
                    $(NumOpcode::$name => Instruction::$name,)*
                }
            }
        }
    };
}

numeric! {
    I32Eqz: [I32] -> I32, Pure;
    I32Eq: [I32, I32] -> I32, Pure;
    I32Ne: [I32, I32] -> I32, Pure;
    I32LtS: [I32, I32] -> I32, Pure;
    I32LtU: [I32, I32] -> I32, Pure;
    I32GtS: [I32, I32] -> I32, Pure;
    I32GtU: [I32, I32] -> I32, Pure;
    I32LeS: [I32, I32] -> I32, Pure;
    I32LeU: [I32, I32] -> I32, Pure;
    I32GeS: [I32, I32] -> I32, Pure;
    I32GeU: [I32, I32] -> I32, Pure;
    I32Clz: [I32] -> I32, Pure;
    I32Ctz: [I32] -> I32, Pure;
    I32Popcnt: [I32] -> I32, Pure;
    I32Add: [I32, I32] -> I32, Pure;
    I32Sub: [I32, I32] -> I32, Pure;
    I32Mul: [I32, I32] -> I32, Pure;
    I32DivS: [I32, I32] -> I32, Traps;
    I32DivU: [I32, I32] -> I32, Traps;
    I32RemS: [I32, I32] -> I32, Traps;
    I32RemU: [I32, I32] -> I32, Traps;
    I32And: [I32, I32] -> I32, Pure;
    I32Or: [I32, I32] -> I32, Pure;
    I32Xor: [I32, I32] -> I32, Pure;
    I32Shl: [I32, I32] -> I32, Pure;
    I32ShrS: [I32, I32] -> I32, Pure;
    I32ShrU: [I32, I32] -> I32, Pure;
    I32Rotl: [I32, I32] -> I32, Pure;
    I32Rotr: [I32, I32] -> I32, Pure;
    I32Extend8S: [I32] -> I32, Pure;
    I32Extend16S: [I32] -> I32, Pure;
    I64Eqz: [I64] -> I32, Pure;
    I64Eq: [I64, I64] -> I32, Pure;
    I64Ne: [I64, I64] -> I32, Pure;
    I64LtS: [I64, I64] -> I32, Pure;
    I64LtU: [I64, I64] -> I32, Pure;
    I64GtS: [I64, I64] -> I32, Pure;
    I64GtU: [I64, I64] -> I32, Pure;
    I64LeS: [I64, I64] -> I32, Pure;
    I64LeU: [I64, I64] -> I32, Pure;
    I64GeS: [I64, I64] -> I32, Pure;
    I64GeU: [I64, I64] -> I32, Pure;
    I64Add: [I64, I64] -> I64, Pure;
    I64Sub: [I64, I64] -> I64, Pure;
    I64Mul: [I64, I64] -> I64, Pure;
    I64DivS: [I64, I64] -> I64, Traps;
    I64DivU: [I64, I64] -> I64, Traps;
    I64RemS: [I64, I64] -> I64, Traps;
    I64RemU: [I64, I64] -> I64, Traps;
    I64And: [I64, I64] -> I64, Pure;
    I64Or: [I64, I64] -> I64, Pure;
    I64Xor: [I64, I64] -> I64, Pure;
    I64Shl: [I64, I64] -> I64, Pure;
    I64ShrS: [I64, I64] -> I64, Pure;
    I64ShrU: [I64, I64] -> I64, Pure;
    F64Eq: [F64, F64] -> I32, Pure;
    F64Ne: [F64, F64] -> I32, Pure;
    F64Lt: [F64, F64] -> I32, Pure;
    F64Gt: [F64, F64] -> I32, Pure;
    F64Le: [F64, F64] -> I32, Pure;
    F64Ge: [F64, F64] -> I32, Pure;
    F64Abs: [F64] -> F64, Pure;
    F64Neg: [F64] -> F64, Pure;
    F64Ceil: [F64] -> F64, Pure;
    F64Floor: [F64] -> F64, Pure;
    F64Trunc: [F64] -> F64, Pure;
    F64Nearest: [F64] -> F64, Pure;
    F64Sqrt: [F64] -> F64, Pure;
    F64Add: [F64, F64] -> F64, Pure;
    F64Sub: [F64, F64] -> F64, Pure;
    F64Mul: [F64, F64] -> F64, Pure;
    F64Div: [F64, F64] -> F64, Pure;
    F64Min: [F64, F64] -> F64, Pure;
    F64Max: [F64, F64] -> F64, Pure;
    F64Copysign: [F64, F64] -> F64, Pure;
    I32WrapI64: [I64] -> I32, Pure;
    I32TruncF64S: [F64] -> I32, Traps;
    I32TruncF64U: [F64] -> I32, Traps;
    I32TruncSatF64S: [F64] -> I32, Pure;
    I32TruncSatF64U: [F64] -> I32, Pure;
    I64ExtendI32S: [I32] -> I64, Pure;
    I64ExtendI32U: [I32] -> I64, Pure;
    I64TruncF64S: [F64] -> I64, Traps;
    I64TruncSatF64S: [F64] -> I64, Pure;
    I64TruncSatF64U: [F64] -> I64, Pure;
    F64ConvertI32S: [I32] -> F64, Pure;
    F64ConvertI32U: [I32] -> F64, Pure;
    F64ConvertI64S: [I64] -> F64, Pure;
    F64ConvertI64U: [I64] -> F64, Pure;
    I64ReinterpretF64: [F64] -> I64, Pure;
    I32ReinterpretF32: [F32] -> I32, Pure;
    F32ReinterpretI32: [I32] -> F32, Pure;
    F64ReinterpretI64: [I64] -> F64, Pure;
    F64PromoteF32: [F32] -> F64, Pure;
    F32DemoteF64: [F64] -> F32, Pure;
}

macro_rules! memory {
    ($($name:ident: $value:ident, $bytes:literal, $load:ident, $store:ident;)*) => {
        /// The value type and width of a linear-memory access. Narrow kinds (`I32U8`, …)
        /// zero-extend or sign-extend on load and truncate the low bits on store.
        #[pliron_attr(name = "wasm.mem_kind", format, verifier = "succ")]
        #[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
        pub enum MemKind {
            $($name,)*
        }

        impl MemKind {
            /// The value type loaded or stored.
            pub(crate) fn value(self) -> ValType {
                match self {
                    $(MemKind::$name => $value,)*
                }
            }

            /// Access width in bytes, which is also the natural alignment.
            pub(crate) fn bytes(self) -> u32 {
                match self {
                    $(MemKind::$name => $bytes,)*
                }
            }

            pub(crate) fn load(self, memarg: MemArg) -> Instruction<'static> {
                match self {
                    $(MemKind::$name => Instruction::$load(memarg),)*
                }
            }

            pub(crate) fn store(self, memarg: MemArg) -> Instruction<'static> {
                match self {
                    $(MemKind::$name => Instruction::$store(memarg),)*
                }
            }
        }
    };
}

memory! {
    I32: I32, 4, I32Load, I32Store;
    I64: I64, 8, I64Load, I64Store;
    F64: F64, 8, F64Load, F64Store;
    F32: F32, 4, F32Load, F32Store;
    I32U8: I32, 1, I32Load8U, I32Store8;
    I32S8: I32, 1, I32Load8S, I32Store8;
    I32U16: I32, 2, I32Load16U, I32Store16;
    I32S16: I32, 2, I32Load16S, I32Store16;
    I64U32: I64, 4, I64Load32U, I64Store32;
}
