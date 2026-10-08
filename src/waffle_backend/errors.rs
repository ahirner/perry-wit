//! Guest Error values and formatting at uncaught exception boundaries.

use super::{objects::ObjectHelpers, runtime, strings::StringHelperFuncs, values::ValueHelpers};
use anyhow::Result;
use perry_hir::ir::Expr;
use std::{collections::BTreeMap, fmt::Write};
use waffle::{Func, Memory, Module};

pub(crate) mod native;

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
    "path",
    "",
    ": ",
    "Uncaught ",
    "undefined",
    "null",
    "true",
    "false",
    "[object Object]",
    "fetch failed",
    "Invalid JSON",
    "Unpaired surrogate in JSON",
    "JSON nesting limit exceeded",
    "JSON storage limit exceeded",
    "Invalid JSON value",
    "Invalid JSON memory range",
    "Invalid UTF-8 in JSON",
    " at byte ",
    " (line ",
    ", column ",
    ")",
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
    pub(crate) native: Func,
    pub(crate) native_data: u32,
    pub(crate) completion: Func,
    pub(crate) cause: Func,
    pub(crate) json: Func,
}

fn runtime_source(pool: &super::strings::StringPool) -> String {
    let mut template = include_str!("errors/runtime.wat");
    let mut source = String::with_capacity(template.len());
    while let Some((prefix, placeholder)) = template.split_once("{{") {
        let (key, rest) = placeholder
            .split_once("}}")
            .expect("closed string-address placeholder");
        source.push_str(prefix);
        let address = if key == "native-errors" {
            pool.next_free_address()
        } else {
            pool.get(key).unwrap()
        };
        write!(source, "{address}").unwrap();
        template = rest;
    }
    source.push_str(template);
    source
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
    let functions = runtime::emit_functions(
        module,
        memory,
        &runtime_source(pool),
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
        native: functions["error.native"],
        native_data: pool.next_free_address(),
        completion: functions["error.completion"],
        cause: functions["error.cause"],
        json: functions["error.json"],
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
