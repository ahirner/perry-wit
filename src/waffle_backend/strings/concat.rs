//! UTF-8 concatenation into checked linear-memory allocations.

use super::descriptor::StringDescriptor;
use anyhow::Result;
use waffle::{
    Func, FuncDecl, FunctionBody, Memory, MemoryArg, Module, Operator, SignatureData, Terminator,
    Type,
};

pub(super) fn emit_concat(
    module: &mut Module<'static>,
    memory: Memory,
    cabi_realloc: Func,
) -> Result<Func> {
    let sig = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::I32],
    });
    let mut body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let a_desc = body.blocks[entry].params[0].1;
    let b_desc = body.blocks[entry].params[1].1;

    let a_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[a_desc],
        &[Type::I32],
    );
    let a_byte_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[a_desc],
        &[Type::I32],
    );
    let a_scalar_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 8,
                memory,
            },
        },
        &[a_desc],
        &[Type::I32],
    );

    let b_ptr = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 0,
                memory,
            },
        },
        &[b_desc],
        &[Type::I32],
    );
    let b_byte_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 4,
                memory,
            },
        },
        &[b_desc],
        &[Type::I32],
    );
    let b_scalar_len = body.add_op(
        entry,
        Operator::I32Load {
            memory: MemoryArg {
                align: 2,
                offset: 8,
                memory,
            },
        },
        &[b_desc],
        &[Type::I32],
    );

    let total_byte_len = body.add_op(
        entry,
        Operator::I32Add,
        &[a_byte_len, b_byte_len],
        &[Type::I32],
    );
    let total_scalar_len = body.add_op(
        entry,
        Operator::I32Add,
        &[a_scalar_len, b_scalar_len],
        &[Type::I32],
    );

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

    body.add_op(
        entry,
        Operator::MemoryCopy {
            dst_mem: memory,
            src_mem: memory,
        },
        &[new_data_ptr, a_ptr, a_byte_len],
        &[],
    );
    let b_destination = body.add_op(
        entry,
        Operator::I32Add,
        &[new_data_ptr, a_byte_len],
        &[Type::I32],
    );
    body.add_op(
        entry,
        Operator::MemoryCopy {
            dst_mem: memory,
            src_mem: memory,
        },
        &[b_destination, b_ptr, b_byte_len],
        &[],
    );

    let new_desc = StringDescriptor {
        data_ptr: new_data_ptr,
        byte_len: total_byte_len,
        scalar_len: total_scalar_len,
    }
    .allocate(&mut body, entry, memory, cabi_realloc);

    body.set_terminator(
        entry,
        Terminator::Return {
            values: vec![new_desc],
        },
    );
    body.validate()?;
    body.verify_reducible()?;
    Ok(module
        .funcs
        .push(FuncDecl::Body(sig, "$rt_str_concat".into(), body)))
}
