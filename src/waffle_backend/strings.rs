//! UTF-8 String Runtime, Storage, and Operation Lowering for WAFFLE Backend.
//!
//! Enforces the Unicode scalar contract:
//! - Strings are represented internally as 12-byte descriptors in linear memory:
//!   `[data_ptr: i32, byte_len: i32, scalar_len: i32]`
//! - Canonical ABI compatibility:
//!   Canonical ABI `(ptr: i32, byte_len: i32)` directly matches descriptor offsets 0 and 4.
//! - Static string literals are interned into initial memory data segments.
//! - Dynamic allocations use a bump allocator (`cabi_realloc`).
//! - String operations (.length, [i], .charAt, .slice, .indexOf, +, comparisons)
//!   operate on Unicode scalar values.

mod allocation;
mod comparison;
mod positions;

use allocation::{PAGE_BYTES, emit_allocator};
use comparison::emit_compare;
use positions::{CharacterAccess, PositionMode, bounded_position, emit_character_access};
use std::collections::BTreeMap;
use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Memory, MemoryArg,
    MemoryData, MemorySegment, Module, Operator, SignatureData, Terminator, Type,
};

/// Base memory address where static string descriptors and data are placed.
pub(crate) const STATIC_STRING_BASE: u32 = 1024;

/// An interned string literal with precalculated byte and scalar lengths.
#[derive(Debug, Clone)]
pub(crate) struct InternedString {
    pub(crate) text: String,
    pub(crate) desc_offset: u32,
    pub(crate) data_offset: u32,
    pub(crate) byte_len: u32,
    pub(crate) scalar_len: u32,
}

/// String pool interning literals and building static memory segments.
#[derive(Debug, Default)]
pub(crate) struct StringPool {
    entries: BTreeMap<String, InternedString>,
    next_offset: u32,
}

impl StringPool {
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_offset: STATIC_STRING_BASE,
        }
    }

    pub(crate) fn get(&self, s: &str) -> Option<u32> {
        self.entries.get(s).map(|e| e.desc_offset)
    }

    /// Intern a string literal, allocating a 12-byte descriptor and data segment.
    pub(crate) fn intern(&mut self, s: &str) -> u32 {
        if let Some(entry) = self.entries.get(s) {
            return entry.desc_offset;
        }

        let desc_offset = self.next_offset;
        let data_offset = desc_offset + 12;
        let bytes = s.as_bytes();
        let byte_len = bytes.len() as u32;
        let scalar_len = s.chars().count() as u32;

        // Advance next_offset aligned to 4 bytes
        let total_size = 12 + byte_len;
        let aligned_size = (total_size + 3) & !3;
        self.next_offset += aligned_size;

        let entry = InternedString {
            text: s.to_string(),
            desc_offset,
            data_offset,
            byte_len,
            scalar_len,
        };
        self.entries.insert(s.to_string(), entry);

        desc_offset
    }

    /// Build static memory data segments for all interned strings.
    pub(crate) fn populate_memory_segments(&self, memory_data: &mut MemoryData) {
        memory_data.initial_pages = memory_data.initial_pages.max(self.next_free_address().div_ceil(PAGE_BYTES) as usize);
        for entry in self.entries.values() {
            // 1. Descriptor: [data_offset: u32, byte_len: u32, scalar_len: u32]
            let mut desc_bytes = Vec::with_capacity(12);
            desc_bytes.extend_from_slice(&entry.data_offset.to_le_bytes());
            desc_bytes.extend_from_slice(&entry.byte_len.to_le_bytes());
            desc_bytes.extend_from_slice(&entry.scalar_len.to_le_bytes());

            memory_data.segments.push(MemorySegment {
                offset: entry.desc_offset as usize,
                data: desc_bytes,
            });

            // 2. UTF-8 byte payload
            if !entry.text.is_empty() {
                memory_data.segments.push(MemorySegment {
                    offset: entry.data_offset as usize,
                    data: entry.text.as_bytes().to_vec(),
                });
            }
        }
    }

    /// Next available memory address after all static strings.
    pub(crate) fn next_free_address(&self) -> u32 {
        self.next_offset.max(STATIC_STRING_BASE)
    }
}

/// Identifiers for synthesized string helper functions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StringHelperFuncs {
    #[allow(dead_code)]
    pub(crate) cabi_realloc: Func,
    pub(crate) lift_canonical: Func,
    pub(crate) str_slice: Func,
    pub(crate) str_char_at: Func,
    pub(crate) str_index: Func,
    pub(crate) str_index_of: Func,
    pub(crate) str_concat: Func,
    pub(crate) str_compare: Func,
}

/// Synthesizes runtime helper functions for strings and memory into the WAFFLE module.
pub(crate) fn emit_string_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    initial_heap_base: u32,
) -> Result<StringHelperFuncs> {
    let cabi_realloc = emit_allocator(module, memory, initial_heap_base)?;

    // 2. lift_canonical(ptr: i32, byte_len: i32) -> desc_ptr: i32
    let lift_canonical = {
        let sig = module.signatures.push(SignatureData {
            params: vec![Type::I32, Type::I32],
            returns: vec![Type::I32],
        });
        let mut body = FunctionBody::new(module, sig);
        let entry = body.entry;
        let ptr = body.blocks[entry].params[0].1;
        let byte_len = body.blocks[entry].params[1].1;

        // Allocate 12 bytes for descriptor
        let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let four = body.add_op(entry, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let twelve = body.add_op(entry, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let alloc_val = body.add_op(
            entry,
            Operator::Call {
                function_index: cabi_realloc,
            },
            &[zero, zero, four, twelve],
            &[Type::I32],
        );
        let desc_ptr = alloc_val;

        // Loop over byte_len to count Unicode scalars:
        // A byte is a scalar lead byte if (b & 0xC0) != 0x80.
        let loop_block = body.add_block();
        body.blocks[loop_block].desc = "lift_canonical count loop".into();
        let loop_idx = body.add_blockparam(loop_block, Type::I32);
        let loop_scalars = body.add_blockparam(loop_block, Type::I32);

        let done_block = body.add_block();
        body.blocks[done_block].desc = "lift_canonical done".into();
        let final_scalars = body.add_blockparam(done_block, Type::I32);

        body.set_terminator(
            entry,
            Terminator::Br {
                target: BlockTarget {
                    block: loop_block,
                    args: vec![zero, zero],
                },
            },
        );

        let is_done = body.add_op(
            loop_block,
            Operator::I32GeU,
            &[loop_idx, byte_len],
            &[Type::I32],
        );
        let step_block = body.add_block();
        body.blocks[step_block].desc = "lift_canonical step".into();

        body.set_terminator(
            loop_block,
            Terminator::CondBr {
                cond: is_done,
                if_true: BlockTarget {
                    block: done_block,
                    args: vec![loop_scalars],
                },
                if_false: BlockTarget {
                    block: step_block,
                    args: vec![],
                },
            },
        );

        let curr_addr = body.add_op(
            step_block,
            Operator::I32Add,
            &[ptr, loop_idx],
            &[Type::I32],
        );
        let b = body.add_op(
            step_block,
            Operator::I32Load8U {
                memory: MemoryArg {
                    align: 0,
                    offset: 0,
                    memory,
                },
            },
            &[curr_addr],
            &[Type::I32],
        );
        let c0 = body.add_op(step_block, Operator::I32Const { value: 0xC0 }, &[], &[Type::I32]);
        let band = body.add_op(step_block, Operator::I32And, &[b, c0], &[Type::I32]);
        let c80 = body.add_op(step_block, Operator::I32Const { value: 0x80 }, &[], &[Type::I32]);
        let is_lead = body.add_op(step_block, Operator::I32Ne, &[band, c80], &[Type::I32]);
        let next_scalars = body.add_op(
            step_block,
            Operator::I32Add,
            &[loop_scalars, is_lead],
            &[Type::I32],
        );

        let one = body.add_op(step_block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let next_idx = body.add_op(step_block, Operator::I32Add, &[loop_idx, one], &[Type::I32]);

        body.set_terminator(
            step_block,
            Terminator::Br {
                target: BlockTarget {
                    block: loop_block,
                    args: vec![next_idx, next_scalars],
                },
            },
        );

        // Store [ptr, byte_len, scalar_len] in desc_ptr
        body.add_op(
            done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory,
                },
            },
            &[desc_ptr, ptr],
            &[],
        );
        body.add_op(
            done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory,
                },
            },
            &[desc_ptr, byte_len],
            &[],
        );
        body.add_op(
            done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 8,
                    memory,
                },
            },
            &[desc_ptr, final_scalars],
            &[],
        );

        body.set_terminator(done_block, Terminator::Return { values: vec![desc_ptr] });
        body.validate()?;
        body.verify_reducible()?;
        module.funcs.push(FuncDecl::Body(sig, "$rt_lift_canonical".into(), body))
    };

    // 3. str_slice(desc: i32, start: f64, end: f64) -> new_desc: i32
    let str_slice = {
        let sig = module.signatures.push(SignatureData {
            params: vec![Type::I32, Type::F64, Type::F64],
            returns: vec![Type::I32],
        });
        let mut body = FunctionBody::new(module, sig);
        let entry = body.entry;
        let desc = body.blocks[entry].params[0].1;
        let start_f64 = body.blocks[entry].params[1].1;
        let end_f64 = body.blocks[entry].params[2].1;

        let ptr = body.add_op(
            entry,
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory,
                },
            },
            &[desc],
            &[Type::I32],
        );
        let byte_len = body.add_op(
            entry,
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory,
                },
            },
            &[desc],
            &[Type::I32],
        );
        let scalar_len = body.add_op(
            entry,
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 8,
                    memory,
                },
            },
            &[desc],
            &[Type::I32],
        );

        let norm_start = bounded_position(&mut body, entry, start_f64, scalar_len, PositionMode::Relative);
        let norm_end = bounded_position(&mut body, entry, end_f64, scalar_len, PositionMode::Relative);
        let zero_i32 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);

        // If norm_start >= norm_end: return empty string descriptor
        let is_empty = body.add_op(entry, Operator::I32GeS, &[norm_start, norm_end], &[Type::I32]);
        let empty_block = body.add_block();
        body.blocks[empty_block].desc = "str_slice empty".into();
        let scan_block = body.add_block();
        body.blocks[scan_block].desc = "str_slice scan".into();

        body.set_terminator(
            entry,
            Terminator::CondBr {
                cond: is_empty,
                if_true: BlockTarget {
                    block: empty_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: scan_block,
                    args: vec![],
                },
            },
        );

        // Empty block: allocate descriptor [ptr, 0, 0]
        let four = body.add_op(empty_block, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let twelve = body.add_op(empty_block, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let empty_desc = body.add_op(
            empty_block,
            Operator::Call {
                function_index: cabi_realloc,
            },
            &[zero_i32, zero_i32, four, twelve],
            &[Type::I32],
        );
        body.add_op(
            empty_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory,
                },
            },
            &[empty_desc, ptr],
            &[],
        );
        body.add_op(
            empty_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory,
                },
            },
            &[empty_desc, zero_i32],
            &[],
        );
        body.add_op(
            empty_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 8,
                    memory,
                },
            },
            &[empty_desc, zero_i32],
            &[],
        );
        body.set_terminator(empty_block, Terminator::Return { values: vec![empty_desc] });

        // Scan block: iterate over byte_len to find byte offsets for norm_start and norm_end
        let loop_block = body.add_block();
        body.blocks[loop_block].desc = "str_slice loop".into();
        let loop_byte_idx = body.add_blockparam(loop_block, Type::I32);
        let loop_scalar_idx = body.add_blockparam(loop_block, Type::I32);
        let loop_start_byte = body.add_blockparam(loop_block, Type::I32);
        let loop_end_byte = body.add_blockparam(loop_block, Type::I32);

        body.set_terminator(
            scan_block,
            Terminator::Br {
                target: BlockTarget {
                    block: loop_block,
                    args: vec![zero_i32, zero_i32, zero_i32, byte_len],
                },
            },
        );

        let at_start = body.add_op(loop_block, Operator::I32Eq, &[loop_scalar_idx, norm_start], &[Type::I32]);
        let new_start_byte = body.add_op(loop_block, Operator::Select, &[loop_byte_idx, loop_start_byte, at_start], &[Type::I32]);

        let at_end = body.add_op(loop_block, Operator::I32Eq, &[loop_scalar_idx, norm_end], &[Type::I32]);
        let new_end_byte = body.add_op(loop_block, Operator::Select, &[loop_byte_idx, loop_end_byte, at_end], &[Type::I32]);

        let is_scan_done = body.add_op(loop_block, Operator::I32GeU, &[loop_byte_idx, byte_len], &[Type::I32]);
        let scan_done_block = body.add_block();
        body.blocks[scan_done_block].desc = "str_slice scan_done".into();
        let scan_step_block = body.add_block();
        body.blocks[scan_step_block].desc = "str_slice scan_step".into();

        body.set_terminator(
            loop_block,
            Terminator::CondBr {
                cond: is_scan_done,
                if_true: BlockTarget {
                    block: scan_done_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: scan_step_block,
                    args: vec![],
                },
            },
        );

        let curr_b_addr = body.add_op(scan_step_block, Operator::I32Add, &[ptr, loop_byte_idx], &[Type::I32]);
        let b_val = body.add_op(
            scan_step_block,
            Operator::I32Load8U {
                memory: MemoryArg {
                    align: 0,
                    offset: 0,
                    memory,
                },
            },
            &[curr_b_addr],
            &[Type::I32],
        );
        let c0 = body.add_op(scan_step_block, Operator::I32Const { value: 0xC0 }, &[], &[Type::I32]);
        let band = body.add_op(scan_step_block, Operator::I32And, &[b_val, c0], &[Type::I32]);
        let c80 = body.add_op(scan_step_block, Operator::I32Const { value: 0x80 }, &[], &[Type::I32]);
        let is_lead_b = body.add_op(scan_step_block, Operator::I32Ne, &[band, c80], &[Type::I32]);
        let next_sc = body.add_op(scan_step_block, Operator::I32Add, &[loop_scalar_idx, is_lead_b], &[Type::I32]);
        let one = body.add_op(scan_step_block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let next_bi = body.add_op(scan_step_block, Operator::I32Add, &[loop_byte_idx, one], &[Type::I32]);

        body.set_terminator(
            scan_step_block,
            Terminator::Br {
                target: BlockTarget {
                    block: loop_block,
                    args: vec![next_bi, next_sc, new_start_byte, new_end_byte],
                },
            },
        );

        // scan_done_block:
        let final_start = new_start_byte;
        let final_end = new_end_byte;
        let slice_byte_len = body.add_op(scan_done_block, Operator::I32Sub, &[final_end, final_start], &[Type::I32]);
        let slice_scalar_len = body.add_op(scan_done_block, Operator::I32Sub, &[norm_end, norm_start], &[Type::I32]);
        let slice_ptr = body.add_op(scan_done_block, Operator::I32Add, &[ptr, final_start], &[Type::I32]);

        let four_val = body.add_op(scan_done_block, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let twelve_val = body.add_op(scan_done_block, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let new_desc = body.add_op(
            scan_done_block,
            Operator::Call {
                function_index: cabi_realloc,
            },
            &[zero_i32, zero_i32, four_val, twelve_val],
            &[Type::I32],
        );
        body.add_op(
            scan_done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory,
                },
            },
            &[new_desc, slice_ptr],
            &[],
        );
        body.add_op(
            scan_done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory,
                },
            },
            &[new_desc, slice_byte_len],
            &[],
        );
        body.add_op(
            scan_done_block,
            Operator::I32Store {
                memory: MemoryArg {
                    align: 2,
                    offset: 8,
                    memory,
                },
            },
            &[new_desc, slice_scalar_len],
            &[],
        );
        body.set_terminator(scan_done_block, Terminator::Return { values: vec![new_desc] });

        body.validate()?;
        body.verify_reducible()?;
        module.funcs.push(FuncDecl::Body(sig, "$rt_str_slice".into(), body))
    };

    let str_char_at = emit_character_access(module, memory, str_slice, CharacterAccess::CharAt)?;
    let str_index = emit_character_access(module, memory, str_slice, CharacterAccess::Index)?;

    // 5. str_concat(a: i32, b: i32) -> i32
    let str_concat = {
        let sig = module.signatures.push(SignatureData {
            params: vec![Type::I32, Type::I32],
            returns: vec![Type::I32],
        });
        let mut body = FunctionBody::new(module, sig);
        let entry = body.entry;
        let a_desc = body.blocks[entry].params[0].1;
        let b_desc = body.blocks[entry].params[1].1;

        let a_ptr = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 0, memory } }, &[a_desc], &[Type::I32]);
        let a_byte_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 4, memory } }, &[a_desc], &[Type::I32]);
        let a_scalar_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 8, memory } }, &[a_desc], &[Type::I32]);

        let b_ptr = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 0, memory } }, &[b_desc], &[Type::I32]);
        let b_byte_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 4, memory } }, &[b_desc], &[Type::I32]);
        let b_scalar_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 8, memory } }, &[b_desc], &[Type::I32]);

        let total_byte_len = body.add_op(entry, Operator::I32Add, &[a_byte_len, b_byte_len], &[Type::I32]);
        let total_scalar_len = body.add_op(entry, Operator::I32Add, &[a_scalar_len, b_scalar_len], &[Type::I32]);

        // Allocate memory for new byte data: cabi_realloc(0, 0, 1, total_byte_len)
        let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let one = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let new_data_ptr = body.add_op(
            entry,
            Operator::Call {
                function_index: cabi_realloc,
            },
            &[zero, zero, one, total_byte_len],
            &[Type::I32],
        );

        // Copy a bytes
        let copy_a_loop = body.add_block();
        body.blocks[copy_a_loop].desc = "concat copy_a loop".into();
        let a_idx = body.add_blockparam(copy_a_loop, Type::I32);

        let copy_b_init = body.add_block();
        body.blocks[copy_b_init].desc = "concat copy_b init".into();

        body.set_terminator(entry, Terminator::Br { target: BlockTarget { block: copy_a_loop, args: vec![zero] } });

        let a_done = body.add_op(copy_a_loop, Operator::I32GeU, &[a_idx, a_byte_len], &[Type::I32]);
        let a_step = body.add_block();
        body.blocks[a_step].desc = "concat copy_a step".into();

        body.set_terminator(
            copy_a_loop,
            Terminator::CondBr {
                cond: a_done,
                if_true: BlockTarget { block: copy_b_init, args: vec![] },
                if_false: BlockTarget { block: a_step, args: vec![] },
            },
        );

        let a_src = body.add_op(a_step, Operator::I32Add, &[a_ptr, a_idx], &[Type::I32]);
        let a_dst = body.add_op(a_step, Operator::I32Add, &[new_data_ptr, a_idx], &[Type::I32]);
        let byte_val = body.add_op(a_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[a_src], &[Type::I32]);
        body.add_op(a_step, Operator::I32Store8 { memory: MemoryArg { align: 0, offset: 0, memory } }, &[a_dst, byte_val], &[]);
        let next_a_idx = body.add_op(a_step, Operator::I32Add, &[a_idx, one], &[Type::I32]);
        body.set_terminator(a_step, Terminator::Br { target: BlockTarget { block: copy_a_loop, args: vec![next_a_idx] } });

        // Copy b bytes
        let copy_b_loop = body.add_block();
        body.blocks[copy_b_loop].desc = "concat copy_b loop".into();
        let b_idx = body.add_blockparam(copy_b_loop, Type::I32);

        let finish_block = body.add_block();
        body.blocks[finish_block].desc = "concat finish".into();

        body.set_terminator(copy_b_init, Terminator::Br { target: BlockTarget { block: copy_b_loop, args: vec![zero] } });

        let b_done = body.add_op(copy_b_loop, Operator::I32GeU, &[b_idx, b_byte_len], &[Type::I32]);
        let b_step = body.add_block();
        body.blocks[b_step].desc = "concat copy_b step".into();

        body.set_terminator(
            copy_b_loop,
            Terminator::CondBr {
                cond: b_done,
                if_true: BlockTarget { block: finish_block, args: vec![] },
                if_false: BlockTarget { block: b_step, args: vec![] },
            },
        );

        let b_src = body.add_op(b_step, Operator::I32Add, &[b_ptr, b_idx], &[Type::I32]);
        let offset_in_dst = body.add_op(b_step, Operator::I32Add, &[a_byte_len, b_idx], &[Type::I32]);
        let b_dst = body.add_op(b_step, Operator::I32Add, &[new_data_ptr, offset_in_dst], &[Type::I32]);
        let b_byte_val = body.add_op(b_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[b_src], &[Type::I32]);
        body.add_op(b_step, Operator::I32Store8 { memory: MemoryArg { align: 0, offset: 0, memory } }, &[b_dst, b_byte_val], &[]);
        let next_b_idx = body.add_op(b_step, Operator::I32Add, &[b_idx, one], &[Type::I32]);
        body.set_terminator(b_step, Terminator::Br { target: BlockTarget { block: copy_b_loop, args: vec![next_b_idx] } });

        // Finish: allocate descriptor [new_data_ptr, total_byte_len, total_scalar_len]
        let four = body.add_op(finish_block, Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let twelve = body.add_op(finish_block, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let new_desc = body.add_op(
            finish_block,
            Operator::Call {
                function_index: cabi_realloc,
            },
            &[zero, zero, four, twelve],
            &[Type::I32],
        );
        body.add_op(finish_block, Operator::I32Store { memory: MemoryArg { align: 2, offset: 0, memory } }, &[new_desc, new_data_ptr], &[]);
        body.add_op(finish_block, Operator::I32Store { memory: MemoryArg { align: 2, offset: 4, memory } }, &[new_desc, total_byte_len], &[]);
        body.add_op(finish_block, Operator::I32Store { memory: MemoryArg { align: 2, offset: 8, memory } }, &[new_desc, total_scalar_len], &[]);

        body.set_terminator(finish_block, Terminator::Return { values: vec![new_desc] });
        body.validate()?;
        body.verify_reducible()?;
        module.funcs.push(FuncDecl::Body(sig, "$rt_str_concat".into(), body))
    };

    // 6. str_index_of(desc: i32, search: i32, pos: f64) -> f64
    let str_index_of = {
        let sig = module.signatures.push(SignatureData {
            params: vec![Type::I32, Type::I32, Type::F64],
            returns: vec![Type::F64],
        });
        let mut body = FunctionBody::new(module, sig);
        let entry = body.entry;
        let desc = body.blocks[entry].params[0].1;
        let search = body.blocks[entry].params[1].1;
        let pos_f64 = body.blocks[entry].params[2].1;

        let ptr = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 0, memory } }, &[desc], &[Type::I32]);
        let byte_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 4, memory } }, &[desc], &[Type::I32]);
        let scalar_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 8, memory } }, &[desc], &[Type::I32]);

        let s_ptr = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 0, memory } }, &[search], &[Type::I32]);
        let s_byte_len = body.add_op(entry, Operator::I32Load { memory: MemoryArg { align: 2, offset: 4, memory } }, &[search], &[Type::I32]);

        let norm_pos = bounded_position(&mut body, entry, pos_f64, scalar_len, PositionMode::Clamped);
        let neg_one_f64 = body.add_op(entry, Operator::F64Const { value: (-1f64).to_bits() }, &[], &[Type::F64]);
        let zero_i32 = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let one = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let c0 = body.add_op(entry, Operator::I32Const { value: 0xC0 }, &[], &[Type::I32]);
        let c80 = body.add_op(entry, Operator::I32Const { value: 0x80 }, &[], &[Type::I32]);

        // If s_byte_len == 0:
        let search_is_empty = body.add_op(entry, Operator::I32Eqz, &[s_byte_len], &[Type::I32]);
        let empty_search_block = body.add_block();
        body.blocks[empty_search_block].desc = "index_of empty search".into();
        let normal_search_block = body.add_block();
        body.blocks[normal_search_block].desc = "index_of normal search".into();

        body.set_terminator(
            entry,
            Terminator::CondBr {
                cond: search_is_empty,
                if_true: BlockTarget { block: empty_search_block, args: vec![] },
                if_false: BlockTarget { block: normal_search_block, args: vec![] },
            },
        );

        // empty_search_block: min(norm_pos, scalar_len) as f64
        let empty_exceeds = body.add_op(empty_search_block, Operator::I32GtS, &[norm_pos, scalar_len], &[Type::I32]);
        let empty_pos_clamped = body.add_op(
            empty_search_block,
            Operator::Select,
            &[scalar_len, norm_pos, empty_exceeds],
            &[Type::I32],
        );
        let empty_res_f64 = body.add_op(empty_search_block, Operator::F64ConvertI32U, &[empty_pos_clamped], &[Type::F64]);
        body.set_terminator(empty_search_block, Terminator::Return { values: vec![empty_res_f64] });

        // normal_search_block: if norm_pos >= scalar_len: return -1.0
        let pos_ge_scalar = body.add_op(normal_search_block, Operator::I32GeS, &[norm_pos, scalar_len], &[Type::I32]);
        let out_of_range_block = body.add_block();
        let find_byte_pos_block = body.add_block();

        body.set_terminator(
            normal_search_block,
            Terminator::CondBr {
                cond: pos_ge_scalar,
                if_true: BlockTarget { block: out_of_range_block, args: vec![] },
                if_false: BlockTarget { block: find_byte_pos_block, args: vec![] },
            },
        );

        body.set_terminator(out_of_range_block, Terminator::Return { values: vec![neg_one_f64] });

        // Loop to find start_byte corresponding to norm_pos
        let find_pos_loop = body.add_block();
        body.blocks[find_pos_loop].desc = "find_start_byte loop".into();
        let f_byte_idx = body.add_blockparam(find_pos_loop, Type::I32);
        let f_scalar_idx = body.add_blockparam(find_pos_loop, Type::I32);

        let search_loop_init = body.add_block();
        body.blocks[search_loop_init].desc = "search_loop init".into();
        let found_start_byte = body.add_blockparam(search_loop_init, Type::I32);

        body.set_terminator(
            find_byte_pos_block,
            Terminator::Br {
                target: BlockTarget { block: find_pos_loop, args: vec![zero_i32, zero_i32] },
            },
        );

        let reached_norm_pos = body.add_op(find_pos_loop, Operator::I32GeS, &[f_scalar_idx, norm_pos], &[Type::I32]);
        let f_step = body.add_block();

        body.set_terminator(
            find_pos_loop,
            Terminator::CondBr {
                cond: reached_norm_pos,
                if_true: BlockTarget { block: search_loop_init, args: vec![f_byte_idx] },
                if_false: BlockTarget { block: f_step, args: vec![] },
            },
        );

        let f_b_addr = body.add_op(f_step, Operator::I32Add, &[ptr, f_byte_idx], &[Type::I32]);
        let f_b = body.add_op(f_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[f_b_addr], &[Type::I32]);
        let f_band = body.add_op(f_step, Operator::I32And, &[f_b, c0], &[Type::I32]);
        let f_is_lead = body.add_op(f_step, Operator::I32Ne, &[f_band, c80], &[Type::I32]);
        let next_f_sc = body.add_op(f_step, Operator::I32Add, &[f_scalar_idx, f_is_lead], &[Type::I32]);
        let next_f_bi = body.add_op(f_step, Operator::I32Add, &[f_byte_idx, one], &[Type::I32]);

        body.set_terminator(
            f_step,
            Terminator::Br {
                target: BlockTarget { block: find_pos_loop, args: vec![next_f_bi, next_f_sc] },
            },
        );

        // Search outer loop: candidate byte index from found_start_byte up to byte_len - s_byte_len
        let outer_loop = body.add_block();
        body.blocks[outer_loop].desc = "index_of outer loop".into();
        let cand_byte = body.add_blockparam(outer_loop, Type::I32);
        let not_found_block = body.add_block();

        body.set_terminator(
            search_loop_init,
            Terminator::Br {
                target: BlockTarget { block: outer_loop, args: vec![found_start_byte] },
            },
        );

        // max_cand = byte_len - s_byte_len
        let max_cand = body.add_op(outer_loop, Operator::I32Sub, &[byte_len, s_byte_len], &[Type::I32]);
        let cand_overflow = body.add_op(outer_loop, Operator::I32GtS, &[cand_byte, max_cand], &[Type::I32]);
        let match_check_block = body.add_block();

        body.set_terminator(
            outer_loop,
            Terminator::CondBr {
                cond: cand_overflow,
                if_true: BlockTarget { block: not_found_block, args: vec![] },
                if_false: BlockTarget { block: match_check_block, args: vec![] },
            },
        );

        body.set_terminator(not_found_block, Terminator::Return { values: vec![neg_one_f64] });

        // Inner loop: check if search bytes match candidate
        let inner_loop = body.add_block();
        body.blocks[inner_loop].desc = "index_of inner cmp loop".into();
        let inner_idx = body.add_blockparam(inner_loop, Type::I32);

        let match_success_block = body.add_block();
        body.blocks[match_success_block].desc = "index_of match success".into();

        body.set_terminator(
            match_check_block,
            Terminator::Br { target: BlockTarget { block: inner_loop, args: vec![zero_i32] } },
        );

        let inner_done = body.add_op(inner_loop, Operator::I32GeU, &[inner_idx, s_byte_len], &[Type::I32]);
        let inner_step = body.add_block();

        body.set_terminator(
            inner_loop,
            Terminator::CondBr {
                cond: inner_done,
                if_true: BlockTarget { block: match_success_block, args: vec![] },
                if_false: BlockTarget { block: inner_step, args: vec![] },
            },
        );

        let h_addr = body.add_op(inner_step, Operator::I32Add, &[ptr, cand_byte], &[Type::I32]);
        let h_char_addr = body.add_op(inner_step, Operator::I32Add, &[h_addr, inner_idx], &[Type::I32]);
        let h_b = body.add_op(inner_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[h_char_addr], &[Type::I32]);

        let n_char_addr = body.add_op(inner_step, Operator::I32Add, &[s_ptr, inner_idx], &[Type::I32]);
        let n_b = body.add_op(inner_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[n_char_addr], &[Type::I32]);

        let chars_match = body.add_op(inner_step, Operator::I32Eq, &[h_b, n_b], &[Type::I32]);
        let inner_next_block = body.add_block();
        let outer_next_block = body.add_block();

        body.set_terminator(
            inner_step,
            Terminator::CondBr {
                cond: chars_match,
                if_true: BlockTarget { block: inner_next_block, args: vec![] },
                if_false: BlockTarget { block: outer_next_block, args: vec![] },
            },
        );

        let next_inner_idx = body.add_op(inner_next_block, Operator::I32Add, &[inner_idx, one], &[Type::I32]);
        body.set_terminator(
            inner_next_block,
            Terminator::Br { target: BlockTarget { block: inner_loop, args: vec![next_inner_idx] } },
        );

        let next_cand_byte = body.add_op(outer_next_block, Operator::I32Add, &[cand_byte, one], &[Type::I32]);
        body.set_terminator(
            outer_next_block,
            Terminator::Br { target: BlockTarget { block: outer_loop, args: vec![next_cand_byte] } },
        );

        // match_success_block: count scalars from 0 up to cand_byte
        let count_loop = body.add_block();
        body.blocks[count_loop].desc = "index_of count scalars".into();
        let c_b_idx = body.add_blockparam(count_loop, Type::I32);
        let c_sc_cnt = body.add_blockparam(count_loop, Type::I32);

        let return_match = body.add_block();
        let ret_sc = body.add_blockparam(return_match, Type::I32);

        body.set_terminator(
            match_success_block,
            Terminator::Br { target: BlockTarget { block: count_loop, args: vec![zero_i32, zero_i32] } },
        );

        let c_done = body.add_op(count_loop, Operator::I32GeU, &[c_b_idx, cand_byte], &[Type::I32]);
        let c_step = body.add_block();

        body.set_terminator(
            count_loop,
            Terminator::CondBr {
                cond: c_done,
                if_true: BlockTarget { block: return_match, args: vec![c_sc_cnt] },
                if_false: BlockTarget { block: c_step, args: vec![] },
            },
        );

        let c_addr = body.add_op(c_step, Operator::I32Add, &[ptr, c_b_idx], &[Type::I32]);
        let c_b = body.add_op(c_step, Operator::I32Load8U { memory: MemoryArg { align: 0, offset: 0, memory } }, &[c_addr], &[Type::I32]);
        let c_band = body.add_op(c_step, Operator::I32And, &[c_b, c0], &[Type::I32]);
        let c_is_lead = body.add_op(c_step, Operator::I32Ne, &[c_band, c80], &[Type::I32]);
        let next_c_sc = body.add_op(c_step, Operator::I32Add, &[c_sc_cnt, c_is_lead], &[Type::I32]);
        let next_c_bi = body.add_op(c_step, Operator::I32Add, &[c_b_idx, one], &[Type::I32]);

        body.set_terminator(
            c_step,
            Terminator::Br { target: BlockTarget { block: count_loop, args: vec![next_c_bi, next_c_sc] } },
        );

        let final_match_f64 = body.add_op(return_match, Operator::F64ConvertI32U, &[ret_sc], &[Type::F64]);
        body.set_terminator(return_match, Terminator::Return { values: vec![final_match_f64] });

        body.validate()?;
        body.verify_reducible()?;
        module.funcs.push(FuncDecl::Body(sig, "$rt_str_index_of".into(), body))
    };

    let str_compare = emit_compare(module, memory)?;

    Ok(StringHelperFuncs {
        cabi_realloc,
        lift_canonical,
        str_slice,
        str_char_at,
        str_index,
        str_index_of,
        str_concat,
        str_compare,
    })
}
