//! Guest Error values and formatting at uncaught exception boundaries.

use super::{objects::ObjectHelpers, runtime, strings::StringHelperFuncs, values::ValueHelpers};
use anyhow::Result;
use perry_hir::ir::Expr;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) const NAMES: [&str; 5] = [
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
];
pub(crate) const KEYS: &[&str] = &[
    "name",
    "message",
    "cause",
    "code",
    "url",
    "",
    ": ",
    "Uncaught ",
    "undefined",
    "null",
    "true",
    "false",
    "[object Object]",
    "Runtime operation failed",
    "fetch failed",
];

pub(crate) fn is_constructor(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::ErrorNew(_)
            | Expr::ErrorNewWithCause { .. }
            | Expr::ErrorNewWithOptions { .. }
            | Expr::TypeErrorNew(_)
            | Expr::RangeErrorNew(_)
            | Expr::ReferenceErrorNew(_)
            | Expr::SyntaxErrorNew(_)
    )
}

#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) new: Func,
    pub(crate) is: Func,
    pub(crate) describe: Func,
    pub(crate) normalize: Func,
    pub(crate) completion: Func,
    pub(crate) cause: Func,
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    strings: StringHelperFuncs,
    objects: ObjectHelpers,
    values: ValueHelpers,
    number_format: Func,
    pool: &super::strings::StringPool,
) -> Result<Helpers> {
    let mut source = include_str!("errors/runtime.wat").to_owned();
    for key in KEYS.iter().copied().chain(NAMES) {
        source = source.replace(
            &format!("{{{{{key}}}}}"),
            &pool.get(key).unwrap().to_string(),
        );
    }
    let functions = runtime::emit_functions(
        module,
        memory,
        &source,
        &BTreeMap::from([
            ("dynamic", objects.dynamic),
            ("record", objects.record),
            ("set", objects.set),
            ("get", objects.get),
            ("box", values.new),
            ("concat", strings.str_concat),
            ("realloc", strings.allocator.realloc),
            ("lift", strings.lift_canonical),
            ("number-format", number_format),
        ]),
    )?;
    Ok(Helpers {
        new: functions["error.new"],
        is: functions["error.is"],
        describe: functions["error.describe"],
        normalize: functions["error.normalize"],
        completion: functions["error.completion"],
        cause: functions["error.cause"],
    })
}

pub(crate) fn emit_report(
    module: &mut Module<'static>,
    memory: Memory,
    errors: Helpers,
    strings: StringHelperFuncs,
    output: Func,
    pool: &super::strings::StringPool,
) -> Result<Func> {
    use super::runtime::builder::{self, Builder};
    use waffle::{
        Operator,
        Type::{F64, I32},
    };
    let function = builder::declare(module, "error.report", &[I32, F64], &[]);
    let mut b = Builder::new(module, function, memory);
    let status = b.param(0);
    let payload = b.param(1);
    let value = b.call(errors.normalize, &[status, payload], &[F64])[0];
    let pointer = b.op(Operator::I32TruncF64U, &[value], I32);
    let message = b.call(errors.describe, &[pointer], &[I32])[0];
    let prefix = b.integer(pool.get("Uncaught ").unwrap());
    let message = b.call(strings.str_concat, &[prefix, message], &[I32])[0];
    b.call(output, &[message], &[I32, F64]);
    b.ret(&[]);
    b.finish(module, function)?;
    Ok(function)
}

pub(crate) fn emit_fetch_failure(
    module: &mut Module<'static>,
    registry: &super::registry::ModuleRegistry,
    pool: &super::strings::StringPool,
    categories: &[&str],
) -> Result<Func> {
    let memory = registry.memory;
    let errors = registry.errors.unwrap();
    let objects = registry.object_helpers.unwrap();
    let values = registry.value_helpers.unwrap();
    let strings = registry.string_helpers.unwrap();
    use super::runtime::builder::{self, Builder};
    use waffle::{
        Operator,
        Type::{F64, I32},
    };
    let function = builder::declare(module, "error.fetch", &[I32, F64, I32], &[I32, F64]);
    let mut b = Builder::new(module, function, memory);
    let status = b.param(0);
    let payload = b.param(1);
    let url = b.param(2);
    let native = b.integer(1);
    let failed = b.op(Operator::I32Eq, &[status, native], I32);
    let failure = b.body.add_block();
    let unchanged = b.body.add_block();
    b.branch(failed, failure, unchanged);
    b.block = unchanged;
    b.ret(&[status, payload]);
    b.block = failure;
    let number = b.integer(3);
    let code = b.call(values.new, &[number, payload], &[I32])[0];
    let mut category = b.call(errors.describe, &[code], &[I32])[0];
    for (index, name) in categories.iter().enumerate() {
        let code = b.number(100.0 + index as f64);
        let matches = b.op(Operator::F64Eq, &[payload, code], I32);
        let name = b.integer(pool.get(name).unwrap());
        category = b.op(Operator::Select, &[name, category, matches], I32);
    }
    let capacity = b.integer(2);
    let cause = b.call(objects.record, &[capacity], &[I32])[0];
    let string = b.integer(4);
    for (key, value) in [("code", category), ("url", url)] {
        let key = b.integer(pool.get(key).unwrap());
        let value = b.op(Operator::F64ConvertI32U, &[value], F64);
        b.call(objects.set, &[cause, key, string, value], &[I32, F64]);
    }
    let object = b.integer(6);
    let cause = b.op(Operator::F64ConvertI32U, &[cause], F64);
    let cause = b.call(values.new, &[object, cause], &[I32])[0];
    let prefix = b.integer(pool.get("fetch failed").unwrap());
    let separator = b.integer(pool.get(": ").unwrap());
    let message = b.call(strings.str_concat, &[prefix, separator], &[I32])[0];
    let message = b.call(strings.str_concat, &[message, category], &[I32])[0];
    let message = b.call(strings.str_concat, &[message, separator], &[I32])[0];
    let message = b.call(strings.str_concat, &[message, url], &[I32])[0];
    let error = b.call(errors.new, &[native, message, cause], &[I32])[0];
    let payload = b.op(Operator::F64ConvertI32U, &[error], F64);
    let thrown = b.integer(3);
    b.ret(&[thrown, payload]);
    b.finish(module, function)?;
    Ok(function)
}
