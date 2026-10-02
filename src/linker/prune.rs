//! Call graph reachability analysis and unused WASI import / function pruning.

use std::collections::VecDeque;

use anyhow::Result;
use wasmparser::{ElementItems, ExternalKind, Global, Operator};

use super::sections::{ParsedModuleA, ParsedModuleB};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuncNode {
    A(usize),
    B(usize),
}

pub(crate) struct PruningPlan {
    /// Old WASI import index -> new index (if Some)
    pub(crate) wasi_old_to_new: Vec<Option<u32>>,
    /// Number of emitted WASI imports
    #[allow(dead_code)]
    pub(crate) num_emitted_wasi: u32,
    /// Old Module B defined function index (relative to b.func_types) -> new index (if Some)
    pub(crate) b_def_old_to_new: Vec<Option<u32>>,
    /// Module B total function map: maps old Module B function index -> new merged function index
    pub(crate) func_map_b: Vec<u32>,
    /// Module A total function map: maps old Module A function index -> new merged function index
    pub(crate) func_map_a: Vec<u32>,
}

pub(crate) fn module_needs_http(a: &ParsedModuleA) -> bool {
    for d in &a.data {
        if d.data
            .windows(b"__needs_http__".len())
            .any(|w| w == b"__needs_http__")
        {
            return true;
        }
    }
    false
}

pub(crate) fn module_needs_clocks(a: &ParsedModuleA) -> bool {
    for d in &a.data {
        if d.data
            .windows(b"__needs_clocks__".len())
            .any(|w| w == b"__needs_clocks__")
        {
            return true;
        }
    }
    false
}

pub(crate) fn module_needs_random(a: &ParsedModuleA) -> bool {
    for d in &a.data {
        if d.data
            .windows(b"__needs_random__".len())
            .any(|w| w == b"__needs_random__")
        {
            return true;
        }
    }
    false
}

pub(crate) fn module_needs_env(a: &ParsedModuleA) -> bool {
    for d in &a.data {
        if d.data
            .windows(b"__needs_env__".len())
            .any(|w| w == b"__needs_env__")
        {
            return true;
        }
    }
    false
}

pub(crate) fn compute_pruning_plan(
    a: &ParsedModuleA,
    b: &ParsedModuleB,
    resolved_imports_a: &[u32],
) -> Result<PruningPlan> {
    let num_wasi = b.wasi_imports.len();
    let num_b_defs = b.func_types.len();
    let num_a_defs = a.func_types.len();
    let num_a_imports = a.imports.len();

    let mut reachable_wasi = vec![false; num_wasi];
    let mut reachable_b_def = vec![false; num_b_defs];
    let mut reachable_a_def = vec![false; num_a_defs];

    let mut worklist = VecDeque::<FuncNode>::new();

    // 1. Module A Roots
    // All defined functions in Module A are kept and scanned.
    for (i, reachable) in reachable_a_def.iter_mut().enumerate() {
        *reachable = true;
        worklist.push_back(FuncNode::A(i));
    }
    // Also mark any exports that map to imports directly (re-exports)
    for exp in &a.exports {
        if exp.kind == ExternalKind::Func {
            mark_a(
                exp.index as usize,
                num_a_imports,
                resolved_imports_a,
                num_wasi,
                num_b_defs,
                num_a_defs,
                &mut worklist,
                &mut reachable_wasi,
                &mut reachable_b_def,
                &mut reachable_a_def,
            );
        }
    }
    for el in &a.elements {
        if let ElementItems::Functions(ref funcs) = el.items {
            for f in funcs.clone() {
                mark_a(
                    f? as usize,
                    num_a_imports,
                    resolved_imports_a,
                    num_wasi,
                    num_b_defs,
                    num_a_defs,
                    &mut worklist,
                    &mut reachable_wasi,
                    &mut reachable_b_def,
                    &mut reachable_a_def,
                );
            }
        }
    }

    // 2. Module B Roots
    // Module B start function (BSS zeroing)
    if let Some(s) = b.start {
        mark_b(
            s as usize,
            num_wasi,
            num_b_defs,
            &mut worklist,
            &mut reachable_wasi,
            &mut reachable_b_def,
        );
    }
    // Module B exports: only cabi_* are preserved roots
    for exp in &b.exports {
        if exp.kind == ExternalKind::Func && exp.name.starts_with("cabi_") {
            mark_b(
                exp.index as usize,
                num_wasi,
                num_b_defs,
                &mut worklist,
                &mut reachable_wasi,
                &mut reachable_b_def,
            );
        }
    }
    // Module B table elements (Rust function pointer table / vtables)
    for el in &b.elements {
        if let ElementItems::Functions(ref funcs) = el.items {
            for f in funcs.clone() {
                mark_b(
                    f? as usize,
                    num_wasi,
                    num_b_defs,
                    &mut worklist,
                    &mut reachable_wasi,
                    &mut reachable_b_def,
                );
            }
        }
    }

    for function in global_function_references(&a.globals)? {
        mark_a(
            function as usize,
            num_a_imports,
            resolved_imports_a,
            num_wasi,
            num_b_defs,
            num_a_defs,
            &mut worklist,
            &mut reachable_wasi,
            &mut reachable_b_def,
            &mut reachable_a_def,
        );
    }
    for function in global_function_references(&b.globals)? {
        mark_b(
            function as usize,
            num_wasi,
            num_b_defs,
            &mut worklist,
            &mut reachable_wasi,
            &mut reachable_b_def,
        );
    }

    // 3. Process Worklist
    while let Some(node) = worklist.pop_front() {
        match node {
            FuncNode::A(a_def) => {
                if let Some(body) = a.bodies.get(a_def) {
                    let mut reader = body.get_operators_reader()?;
                    while !reader.eof() {
                        match reader.read()? {
                            Operator::Call { function_index }
                            | Operator::ReturnCall { function_index }
                            | Operator::RefFunc { function_index } => {
                                mark_a(
                                    function_index as usize,
                                    num_a_imports,
                                    resolved_imports_a,
                                    num_wasi,
                                    num_b_defs,
                                    num_a_defs,
                                    &mut worklist,
                                    &mut reachable_wasi,
                                    &mut reachable_b_def,
                                    &mut reachable_a_def,
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            FuncNode::B(b_def) => {
                if let Some(body) = b.bodies.get(b_def) {
                    let mut reader = body.get_operators_reader()?;
                    while !reader.eof() {
                        match reader.read()? {
                            Operator::Call { function_index }
                            | Operator::ReturnCall { function_index }
                            | Operator::RefFunc { function_index } => {
                                mark_b(
                                    function_index as usize,
                                    num_wasi,
                                    num_b_defs,
                                    &mut worklist,
                                    &mut reachable_wasi,
                                    &mut reachable_b_def,
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // 4. Build Index Spaces
    let mut wasi_old_to_new = vec![None; num_wasi];
    let mut num_emitted_wasi = 0u32;
    for (i, &reachable) in reachable_wasi.iter().enumerate() {
        if reachable {
            wasi_old_to_new[i] = Some(num_emitted_wasi);
            num_emitted_wasi += 1;
        }
    }

    // Module A defined functions occupy [num_emitted_wasi .. num_emitted_wasi + num_a_defs]
    let num_a_funcs = num_a_defs as u32;

    // Module B defined functions occupy [num_emitted_wasi + num_a_funcs .. ]
    let mut b_def_old_to_new = vec![None; num_b_defs];
    let mut num_emitted_b_defs = 0u32;
    for (j, &reachable) in reachable_b_def.iter().enumerate() {
        if reachable {
            b_def_old_to_new[j] = Some(num_emitted_wasi + num_a_funcs + num_emitted_b_defs);
            num_emitted_b_defs += 1;
        }
    }

    // Module B Function Map: maps any old Module B function index to its new index
    let mut func_map_b = Vec::with_capacity(num_wasi + num_b_defs);
    for index in wasi_old_to_new.iter().chain(&b_def_old_to_new) {
        func_map_b.push(index.unwrap_or(0));
    }

    // Module A Function Map: maps any old Module A function index to its new index
    let mut func_map_a = Vec::with_capacity(num_a_imports + num_a_defs);
    for &target in resolved_imports_a {
        let b_target = target as usize;
        func_map_a.push(func_map_b[b_target]);
    }
    for i in 0..num_a_defs {
        func_map_a.push(num_emitted_wasi + (i as u32));
    }

    Ok(PruningPlan {
        wasi_old_to_new,
        num_emitted_wasi,
        b_def_old_to_new,
        func_map_b,
        func_map_a,
    })
}

/// Finds roots in global initializers, which survive function pruning.
fn global_function_references(globals: &[Global<'_>]) -> Result<Vec<u32>> {
    let mut functions = Vec::new();
    for global in globals {
        let mut reader = global.init_expr.get_operators_reader();
        while !reader.eof() {
            if let Operator::RefFunc { function_index } = reader.read()? {
                functions.push(function_index);
            }
        }
    }
    Ok(functions)
}

#[expect(clippy::too_many_arguments)]
fn mark_a(
    idx: usize,
    num_a_imports: usize,
    resolved_imports_a: &[u32],
    num_wasi: usize,
    num_b_defs: usize,
    num_a_defs: usize,
    worklist: &mut VecDeque<FuncNode>,
    reachable_wasi: &mut [bool],
    reachable_b_def: &mut [bool],
    reachable_a_def: &mut [bool],
) {
    if idx < num_a_imports {
        let b_target = resolved_imports_a[idx] as usize;
        if b_target < num_wasi {
            reachable_wasi[b_target] = true;
        } else {
            let b_def = b_target - num_wasi;
            if b_def < num_b_defs && !reachable_b_def[b_def] {
                reachable_b_def[b_def] = true;
                worklist.push_back(FuncNode::B(b_def));
            }
        }
    } else {
        let a_def = idx - num_a_imports;
        if a_def < num_a_defs && !reachable_a_def[a_def] {
            reachable_a_def[a_def] = true;
            worklist.push_back(FuncNode::A(a_def));
        }
    }
}

fn mark_b(
    idx: usize,
    num_wasi: usize,
    num_b_defs: usize,
    worklist: &mut VecDeque<FuncNode>,
    reachable_wasi: &mut [bool],
    reachable_b_def: &mut [bool],
) {
    if idx < num_wasi {
        reachable_wasi[idx] = true;
    } else {
        let b_def = idx - num_wasi;
        if b_def < num_b_defs && !reachable_b_def[b_def] {
            reachable_b_def[b_def] = true;
            worklist.push_back(FuncNode::B(b_def));
        }
    }
}
