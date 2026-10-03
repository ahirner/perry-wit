//! UTF-8 text operations backed by guest helpers.

use super::descriptor::StringDescriptor;
use crate::waffle_backend::link::HELPER_MODULE;
use anyhow::Result;
use waffle::{
    BlockTarget, Func, FuncDecl, FunctionBody, Import, ImportKind, Memory, MemoryArg, Module,
    Operator, SignatureData, Terminator, Type,
};

pub(crate) struct TextImports {
    pub(crate) str_code_point_at: Func,
    pub(crate) str_from_code_point: Func,
    pub(crate) str_case_convert: Func,
    pub(crate) str_split_count: Func,
    pub(crate) str_split_populate: Func,
    pub(crate) str_join_total_len: Func,
    pub(crate) str_join: Func,
}

pub(super) fn declare_text_imports(module: &mut Module<'static>) -> Result<TextImports> {
    // 1. str_code_point_at: (i32, i32, f64) -> f64
    let sig_cpa = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::F64],
        returns: vec![Type::F64],
    });
    let func_cpa = module
        .funcs
        .push(FuncDecl::Import(sig_cpa, "str_code_point_at".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_code_point_at".into(),
        kind: ImportKind::Func(func_cpa),
    });

    // 2. str_from_code_point: (f64, i32) -> i32
    let sig_fcp = module.signatures.push(SignatureData {
        params: vec![Type::F64, Type::I32],
        returns: vec![Type::I32],
    });
    let func_fcp = module
        .funcs
        .push(FuncDecl::Import(sig_fcp, "str_from_code_point".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_from_code_point".into(),
        kind: ImportKind::Func(func_fcp),
    });

    // 3. str_case_convert: (i32, i32, i32, i32) -> i64
    let sig_cc = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32, Type::I32],
        returns: vec![Type::I64],
    });
    let func_cc = module
        .funcs
        .push(FuncDecl::Import(sig_cc, "str_case_convert".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_case_convert".into(),
        kind: ImportKind::Func(func_cc),
    });

    // 4. str_split_count: (i32, i32, i32, i32) -> i32
    let sig_sc = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let func_sc = module
        .funcs
        .push(FuncDecl::Import(sig_sc, "str_split_count".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_split_count".into(),
        kind: ImportKind::Func(func_sc),
    });

    // 5. str_split_populate: (i32, i32, i32, i32, i32) -> i32
    let sig_sp = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32, Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let func_sp = module
        .funcs
        .push(FuncDecl::Import(sig_sp, "str_split_populate".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_split_populate".into(),
        kind: ImportKind::Func(func_sp),
    });

    // 6. str_join_total_len: (i32, i32, i32) -> i32
    let sig_jtl = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let func_jtl = module
        .funcs
        .push(FuncDecl::Import(sig_jtl, "str_join_total_len".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_join_total_len".into(),
        kind: ImportKind::Func(func_jtl),
    });

    // 7. str_join: (i32, i32, i32, i32, i32) -> i64
    let sig_sj = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32, Type::I32, Type::I32],
        returns: vec![Type::I64],
    });
    let func_sj = module
        .funcs
        .push(FuncDecl::Import(sig_sj, "str_join".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_join".into(),
        kind: ImportKind::Func(func_sj),
    });

    Ok(TextImports {
        str_code_point_at: func_cpa,
        str_from_code_point: func_fcp,
        str_case_convert: func_cc,
        str_split_count: func_sc,
        str_split_populate: func_sp,
        str_join_total_len: func_jtl,
        str_join: func_sj,
    })
}

/// Emits wrapper for codePointAt: (desc: i32, pos: f64) -> f64
pub(super) fn emit_code_point_at(
    module: &mut Module<'static>,
    memory: Memory,
    helper: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::F64],
        returns: vec![Type::F64],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let pos = body.blocks[entry].params[1].1;

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

    let res = body.add_op(
        entry,
        Operator::Call {
            function_index: helper,
        },
        &[ptr, byte_len, pos],
        &[Type::F64],
    );

    body.set_terminator(entry, Terminator::Return { values: vec![res] });
    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_code_point_at".into(), body)))
}

/// Emits wrapper for fromCodePoint: (code_point: f64) -> i32 (string descriptor)
pub(super) fn emit_from_code_point(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
    helper: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::F64],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let code_point = body.blocks[entry].params[0].1;

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let align = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let size = body.add_op(entry, Operator::I32Const { value: 4 }, &[], &[Type::I32]);

    let data_ptr = body.add_op(
        entry,
        Operator::Call {
            function_index: cabi_realloc,
        },
        &[zero, zero, align, size],
        &[Type::I32],
    );

    let byte_len = body.add_op(
        entry,
        Operator::Call {
            function_index: helper,
        },
        &[code_point, data_ptr],
        &[Type::I32],
    );

    let err_val = body.add_op(
        entry,
        Operator::I32Const { value: u32::MAX },
        &[],
        &[Type::I32],
    );
    let is_err = body.add_op(entry, Operator::I32Eq, &[byte_len, err_val], &[Type::I32]);

    let ok_block = body.add_block();
    let trap_block = body.add_block();

    body.set_terminator(
        entry,
        Terminator::CondBr {
            cond: is_err,
            if_true: BlockTarget {
                block: trap_block,
                args: vec![],
            },
            if_false: BlockTarget {
                block: ok_block,
                args: vec![],
            },
        },
    );

    body.set_terminator(trap_block, Terminator::Unreachable);

    let one = body.add_op(ok_block, Operator::I32Const { value: 1 }, &[], &[Type::I32]);
    let desc = StringDescriptor {
        data_ptr,
        byte_len,
        scalar_len: one,
    }
    .allocate(&mut body, ok_block, memory, cabi_realloc);

    body.set_terminator(ok_block, Terminator::Return { values: vec![desc] });

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_from_code_point".into(), body)))
}

/// Emits wrapper for case convert: (desc: i32, to_upper: i32) -> i32 (string descriptor)
pub(super) fn emit_case_convert(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
    helper: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let to_upper = body.blocks[entry].params[1].1;

    let src_ptr = body.add_op(
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
    let src_byte_len = body.add_op(
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

    // Allocate buffer: src_byte_len * 2 + 16
    let two = body.add_op(entry, Operator::I32Const { value: 2 }, &[], &[Type::I32]);
    let double_len = body.add_op(entry, Operator::I32Mul, &[src_byte_len, two], &[Type::I32]);
    let sixteen = body.add_op(entry, Operator::I32Const { value: 16 }, &[], &[Type::I32]);
    let alloc_size = body.add_op(entry, Operator::I32Add, &[double_len, sixteen], &[Type::I32]);

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let align = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);

    let out_buf = body.add_op(
        entry,
        Operator::Call {
            function_index: cabi_realloc,
        },
        &[zero, zero, align, alloc_size],
        &[Type::I32],
    );

    let res64 = body.add_op(
        entry,
        Operator::Call {
            function_index: helper,
        },
        &[src_ptr, src_byte_len, out_buf, to_upper],
        &[Type::I64],
    );

    let out_byte_len = body.add_op(entry, Operator::I32WrapI64, &[res64], &[Type::I32]);
    let thirty_two = body.add_op(entry, Operator::I64Const { value: 32 }, &[], &[Type::I64]);
    let shifted = body.add_op(entry, Operator::I64ShrU, &[res64, thirty_two], &[Type::I64]);
    let out_scalar_len = body.add_op(entry, Operator::I32WrapI64, &[shifted], &[Type::I32]);

    let new_desc = StringDescriptor {
        data_ptr: out_buf,
        byte_len: out_byte_len,
        scalar_len: out_scalar_len,
    }
    .allocate(&mut body, entry, memory, cabi_realloc);

    body.set_terminator(entry, Terminator::Return { values: vec![new_desc] });

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_case_convert".into(), body)))
}

/// Emits wrapper for split: (desc: i32, sep_desc: i32) -> i32 (array descriptor [elements_ptr, count])
pub(super) fn emit_split(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
    count_helper: Func,
    populate_helper: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let sep_desc = body.blocks[entry].params[1].1;

    let h_ptr = body.add_op(
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
    let h_len = body.add_op(
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

    let s_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[sep_desc],
        &[Type::I32],
    );
    let s_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[sep_desc],
        &[Type::I32],
    );

    let count = body.add_op(
        entry,
        Operator::Call {
            function_index: count_helper,
        },
        &[h_ptr, h_len, s_ptr, s_len],
        &[Type::I32],
    );

    // Allocate array buffer: 8 bytes header + count * 12 bytes
    let twelve = body.add_op(entry, Operator::I32Const { value: 12 }, &[], &[Type::I32]);
    let items_bytes = body.add_op(entry, Operator::I32Mul, &[count, twelve], &[Type::I32]);
    let eight = body.add_op(entry, Operator::I32Const { value: 8 }, &[], &[Type::I32]);
    let total_alloc = body.add_op(entry, Operator::I32Add, &[items_bytes, eight], &[Type::I32]);

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let align = body.add_op(entry, Operator::I32Const { value: 4 }, &[], &[Type::I32]);

    let arr_ptr = body.add_op(
        entry,
        Operator::Call {
            function_index: cabi_realloc,
        },
        &[zero, zero, align, total_alloc],
        &[Type::I32],
    );

    let elements_ptr = body.add_op(entry, Operator::I32Add, &[arr_ptr, eight], &[Type::I32]);

    body.add_op(
        entry,
        Operator::Call {
            function_index: populate_helper,
        },
        &[h_ptr, h_len, s_ptr, s_len, elements_ptr],
        &[Type::I32],
    );

    // Store [elements_ptr: i32, count: i32] into arr_ptr
    body.add_op(
        entry,
        Operator::I32Store {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[arr_ptr, elements_ptr],
        &[],
    );
    body.add_op(
        entry,
        Operator::I32Store {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[arr_ptr, count],
        &[],
    );

    body.set_terminator(entry, Terminator::Return { values: vec![arr_ptr] });

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_split".into(), body)))
}

/// Emits wrapper for join: (arr_ptr: i32, sep_desc: i32) -> i32 (string descriptor)
pub(super) fn emit_join(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
    total_len_helper: Func,
    join_helper: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let arr_ptr = body.blocks[entry].params[0].1;
    let sep_desc = body.blocks[entry].params[1].1;

    let elements_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[arr_ptr],
        &[Type::I32],
    );
    let count = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[arr_ptr],
        &[Type::I32],
    );

    let s_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[sep_desc],
        &[Type::I32],
    );
    let s_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[sep_desc],
        &[Type::I32],
    );

    let needed_bytes = body.add_op(
        entry,
        Operator::Call {
            function_index: total_len_helper,
        },
        &[elements_ptr, count, s_len],
        &[Type::I32],
    );

    let zero = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
    let align = body.add_op(entry, Operator::I32Const { value: 1 }, &[], &[Type::I32]);

    let out_buf = body.add_op(
        entry,
        Operator::Call {
            function_index: cabi_realloc,
        },
        &[zero, zero, align, needed_bytes],
        &[Type::I32],
    );

    let res64 = body.add_op(
        entry,
        Operator::Call {
            function_index: join_helper,
        },
        &[elements_ptr, count, s_ptr, s_len, out_buf],
        &[Type::I64],
    );

    let out_byte_len = body.add_op(entry, Operator::I32WrapI64, &[res64], &[Type::I32]);
    let thirty_two = body.add_op(entry, Operator::I64Const { value: 32 }, &[], &[Type::I64]);
    let shifted = body.add_op(entry, Operator::I64ShrU, &[res64, thirty_two], &[Type::I64]);
    let out_scalar_len = body.add_op(entry, Operator::I32WrapI64, &[shifted], &[Type::I32]);

    let new_desc = StringDescriptor {
        data_ptr: out_buf,
        byte_len: out_byte_len,
        scalar_len: out_scalar_len,
    }
    .allocate(&mut body, entry, memory, cabi_realloc);

    body.set_terminator(entry, Terminator::Return { values: vec![new_desc] });

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_join".into(), body)))
}
