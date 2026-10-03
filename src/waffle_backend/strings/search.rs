//! UTF-8 search with scalar input and output positions.

use super::positions::{PositionMode, bounded_position};
use crate::waffle_backend::link::HELPER_MODULE;
use anyhow::Result;
use waffle::{
    Func, FuncDecl, FunctionBody, Import, ImportKind, Memory, MemoryArg, Module, Operator,
    SignatureData, Terminator, Type,
};

pub(super) fn declare_index_of_import(module: &mut Module<'static>) -> Result<Func> {
    let helper_sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::I32, Type::I32, Type::I32],
        returns: vec![Type::F64],
    });
    let helper_func = module
        .funcs
        .push(FuncDecl::Import(helper_sig, "str_find_substring".into()));
    module.imports.push(Import {
        module: HELPER_MODULE.into(),
        name: "str_find_substring".into(),
        kind: ImportKind::Func(helper_func),
    });
    Ok(helper_func)
}

pub(super) fn emit_index_of(
    module: &mut Module<'static>,
    memory: Memory,
    helper_func: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32, Type::F64],
        returns: vec![Type::F64],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let desc = body.blocks[entry].params[0].1;
    let search = body.blocks[entry].params[1].1;
    let pos_f64 = body.blocks[entry].params[2].1;

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

    let s_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[search],
        &[Type::I32],
    );
    let s_byte_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[search],
        &[Type::I32],
    );

    let norm_pos = bounded_position(&mut body, entry, pos_f64, scalar_len, PositionMode::Clamped);

    let res = body.add_op(
        entry,
        Operator::Call {
            function_index: helper_func,
        },
        &[ptr, byte_len, s_ptr, s_byte_len, norm_pos],
        &[Type::F64],
    );

    body.set_terminator(entry, Terminator::Return { values: vec![res] });

    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_index_of".into(), body)))
}
