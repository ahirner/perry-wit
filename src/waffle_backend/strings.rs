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
pub(crate) use allocation::emit_post_return;
mod canonical;
mod comparison;
mod concat;
mod descriptor;
mod positions;
pub(crate) use positions::valid_index;
mod search;
mod slicing;
mod text_ops;

use std::collections::BTreeMap;

use anyhow::Result;
use waffle::{Func, Memory, MemoryData, MemorySegment, Module};

use allocation::{PAGE_BYTES, emit_allocator};
use canonical::emit_lift;
use comparison::emit_compare;
use concat::emit_concat;
use positions::{CharacterAccess, emit_character_access};
use search::{declare_index_of_import, emit_index_of};
use slicing::emit_slice;
use text_ops::{
    declare_case_convert_import, declare_code_point_at_import, declare_from_code_point_import,
    declare_join_imports, declare_split_imports, emit_case_convert, emit_code_point_at,
    emit_from_code_point, emit_join, emit_split,
};

/// Track which string helpers are requested by the module.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequiredStringHelpers {
    pub needs_strings: bool,
    pub find_substring: bool,
    pub code_point_at: bool,
    pub from_code_point: bool,
    pub case_convert: bool,
    pub split: bool,
    pub join: bool,
}

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
#[derive(Debug)]
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
        memory_data.initial_pages = memory_data
            .initial_pages
            .max(self.next_free_address().div_ceil(PAGE_BYTES) as usize);
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
    pub(crate) lift_canonical: Func,
    pub(crate) str_slice: Func,
    pub(crate) str_char_at: Func,
    pub(crate) str_index: Func,
    pub(crate) str_index_of: Option<Func>,
    pub(crate) str_concat: Func,
    pub(crate) str_compare: Func,
    #[allow(dead_code)]
    pub(crate) cabi_realloc: Func,
    pub(crate) str_code_point_at: Option<Func>,
    pub(crate) str_from_code_point: Option<Func>,
    pub(crate) str_case_convert: Option<Func>,
    pub(crate) str_split: Option<Func>,
    pub(crate) str_join: Option<Func>,
}

/// Synthesizes runtime helper functions for strings and memory into the WAFFLE module.
pub(crate) fn emit_string_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    initial_heap_base: u32,
    reqs: RequiredStringHelpers,
) -> Result<StringHelperFuncs> {
    // Declare all required helper imports BEFORE emitting any function bodies
    let str_find_helper = if reqs.find_substring {
        Some(declare_index_of_import(module)?)
    } else {
        None
    };
    let code_point_at_import = if reqs.code_point_at {
        Some(declare_code_point_at_import(module)?)
    } else {
        None
    };
    let from_code_point_import = if reqs.from_code_point {
        Some(declare_from_code_point_import(module)?)
    } else {
        None
    };
    let case_convert_import = if reqs.case_convert {
        Some(declare_case_convert_import(module)?)
    } else {
        None
    };
    let split_imports = if reqs.split {
        Some(declare_split_imports(module)?)
    } else {
        None
    };
    let join_imports = if reqs.join {
        Some(declare_join_imports(module)?)
    } else {
        None
    };

    // Emit core function bodies
    let cabi_realloc = emit_allocator(module, memory, initial_heap_base)?;
    let lift_canonical = emit_lift(module, memory, cabi_realloc)?;
    let str_slice = emit_slice(module, memory, cabi_realloc)?;
    let str_char_at = emit_character_access(module, memory, str_slice, CharacterAccess::CharAt)?;
    let str_index = emit_character_access(module, memory, str_slice, CharacterAccess::Index)?;
    let str_concat = emit_concat(module, memory, cabi_realloc)?;
    let str_compare = emit_compare(module, memory)?;

    let str_index_of = if let Some(import) = str_find_helper {
        Some(emit_index_of(module, memory, import)?)
    } else {
        None
    };

    let str_code_point_at = if let Some(import) = code_point_at_import {
        Some(emit_code_point_at(module, memory, import)?)
    } else {
        None
    };

    let str_from_code_point = if let Some(import) = from_code_point_import {
        Some(emit_from_code_point(module, memory, cabi_realloc, import)?)
    } else {
        None
    };

    let str_case_convert = if let Some(import) = case_convert_import {
        Some(emit_case_convert(module, memory, cabi_realloc, import)?)
    } else {
        None
    };

    let str_split = if let Some((cnt, pop)) = split_imports {
        Some(emit_split(module, memory, cabi_realloc, cnt, pop)?)
    } else {
        None
    };

    let str_join = if let Some((tot, jn)) = join_imports {
        Some(emit_join(module, memory, cabi_realloc, tot, jn)?)
    } else {
        None
    };

    Ok(StringHelperFuncs {
        lift_canonical,
        str_slice,
        str_char_at,
        str_index,
        str_index_of,
        str_concat,
        str_compare,
        cabi_realloc,
        str_code_point_at,
        str_from_code_point,
        str_case_convert,
        str_split,
        str_join,
    })
}
