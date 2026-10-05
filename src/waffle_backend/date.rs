//! Date storage, timestamp clipping, UTC calendar arithmetic, and ISO formatting.

use super::{
    allocation::AllocationFuncs,
    runtime::builder::{self, Builder},
};
use anyhow::Result;
use perry_hir::{
    ir::{Expr, Module as HirModule},
    types::Type as HirType,
};
use waffle::{Func, FuncDecl, Import, ImportKind, Memory, Module, Operator, SignatureData, Type};

pub(crate) const DATE_TYPE: &str = "__perry_internal_date";

#[derive(Clone, Copy)]
pub(crate) struct DateHelpers {
    pub(crate) new: Func,
    pub(crate) iso: Func,
    pub(crate) parse: Func,
    pub(crate) clip: Func,
    pub(crate) part: Func,
}

#[derive(Clone, Copy)]
pub(crate) struct DateImports {
    pub(crate) iso: Func,
    pub(crate) parse: Func,
    pub(crate) part: Func,
}

pub(crate) fn is_date(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == DATE_TYPE)
}

pub(crate) fn required(hir: &HirModule) -> bool {
    hir.functions.iter().any(|function| {
        let mut required = contains_date(&function.return_type)
            || function.params.iter().any(|param| contains_date(&param.ty));
        super::visit::visit_function_expressions(function, &mut |expression| {
            if let Expr::ExternFuncRef { return_type, .. } = expression {
                required |= is_date(return_type);
            }
        });
        required
    })
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> DateImports {
    let sig_iso = module.signatures.push(SignatureData {
        params: vec![Type::F64, Type::I32, Type::I32],
        returns: vec![Type::I64],
    });
    let iso = module
        .funcs
        .push(FuncDecl::Import(sig_iso, "time_date_iso".into()));
    module.imports.push(Import {
        module: super::link::HELPER_MODULE.into(),
        name: "time_date_iso".into(),
        kind: ImportKind::Func(iso),
    });

    let sig_parse = module.signatures.push(SignatureData {
        params: vec![Type::I32, Type::I32],
        returns: vec![Type::F64],
    });
    let parse = module
        .funcs
        .push(FuncDecl::Import(sig_parse, "time_date_parse".into()));
    module.imports.push(Import {
        module: super::link::HELPER_MODULE.into(),
        name: "time_date_parse".into(),
        kind: ImportKind::Func(parse),
    });

    let sig_part = module.signatures.push(SignatureData {
        params: vec![Type::F64, Type::I32],
        returns: vec![Type::F64],
    });
    let part = module
        .funcs
        .push(FuncDecl::Import(sig_part, "time_date_part".into()));
    module.imports.push(Import {
        module: super::link::HELPER_MODULE.into(),
        name: "time_date_part".into(),
        kind: ImportKind::Func(part),
    });

    DateImports { iso, parse, part }
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: DateImports,
) -> Result<DateHelpers> {
    use Type::{F64, I32, I64};
    let clip = builder::declare(module, "date.clip", &[F64], &[F64]);
    let mut b = Builder::new(module, clip, memory);
    let time = b.param(0);
    let absolute = b.op(Operator::F64Abs, &[time], F64);
    let limit = b.op(
        Operator::F64Const {
            value: 8_640_000_000_000_000_f64.to_bits(),
        },
        &[],
        F64,
    );
    let valid = b.op(Operator::F64Le, &[absolute, limit], I32);
    let time = b.op(Operator::F64Trunc, &[time], F64);
    let positive_zero = b.op(Operator::F64Const { value: 0 }, &[], F64);
    let time = b.op(Operator::F64Add, &[time, positive_zero], F64);
    let invalid = b.op(
        Operator::F64Const {
            value: f64::NAN.to_bits(),
        },
        &[],
        F64,
    );
    let time = b.op(Operator::Select, &[time, invalid, valid], F64);
    b.ret(&[time]);
    b.finish(module, clip)?;

    let new = builder::declare(module, "date.new", &[F64], &[I32]);
    let mut b = Builder::new(module, new, memory);
    let time = b.call(clip, &[b.param(0)], &[F64])[0];
    let zero = b.integer(0);
    let eight = b.integer(8);
    let date = b.call(allocator.realloc, &[zero, zero, eight, eight], &[I32])[0];
    b.store(date, 0, time, F64);
    b.ret(&[date]);
    b.finish(module, new)?;

    let iso = builder::declare(module, "date.iso", &[I32], &[I32, F64]);
    let mut b = Builder::new(module, iso, memory);
    let time = b.load(b.param(0), 0, F64);
    let zero = b.integer(0);
    let four = b.integer(4);
    let size = b.integer(39);
    let string = b.call(allocator.realloc, &[zero, zero, four, size], &[I32])[0];
    let twelve = b.integer(12);
    let output = b.op(Operator::I32Add, &[string, twelve], I32);
    let capacity = b.integer(27);
    let packed = b.call(imports.iso, &[time, output, capacity], &[I64])[0];
    let shift = b.op(Operator::I64Const { value: 32 }, &[], I64);
    let error = b.op(Operator::I64ShrU, &[packed, shift], I64);
    let error = b.op(Operator::I32WrapI64, &[error], I32);
    let failed = b.body.add_block();
    let success = b.body.add_block();
    b.branch(error, failed, success);
    b.block = failed;
    let tag = b.integer(1);
    let error = b.op(
        Operator::F64Const {
            value: 1_f64.to_bits(),
        },
        &[],
        F64,
    );
    b.ret(&[tag, error]);
    b.block = success;
    let length = b.op(Operator::I32WrapI64, &[packed], I32);
    let header = b.op(Operator::I32Sub, &[string, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(1);
    b.store(header, 16, kind, I32);
    b.store(string, 0, output, I32);
    b.store(string, 4, length, I32);
    b.store(string, 8, length, I32);
    let payload = b.op(Operator::F64ConvertI32U, &[string], F64);
    b.ret(&[zero, payload]);
    b.finish(module, iso)?;
    Ok(DateHelpers {
        new,
        iso,
        parse: imports.parse,
        clip,
        part: imports.part,
    })
}

fn contains_date(ty: &HirType) -> bool {
    match ty {
        HirType::Promise(inner) | HirType::Array(inner) => contains_date(inner),
        HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(contains_date),
        HirType::Object(fields) => {
            fields
                .properties
                .values()
                .any(|property| contains_date(&property.ty))
                || fields.index_signature.as_deref().is_some_and(contains_date)
        }
        _ => is_date(ty),
    }
}
