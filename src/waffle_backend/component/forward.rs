//! Resolve canonical adapters that require an already-instantiated guest memory.

use std::fmt::Write;

use anyhow::Result;

pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) params: Vec<&'static str>,
    pub(crate) results: Vec<&'static str>,
    pub(crate) target: String,
}

pub(crate) fn declare(name: &str, functions: &[Function]) -> Result<String> {
    let mut wat = format!(
        "(core module ${name}-forward (table (export \"table\") {} funcref)\n",
        functions.len()
    );
    for (index, function) in functions.iter().enumerate() {
        let params = function.params.join(" ");
        let results = function.results.join(" ");
        writeln!(
            wat,
            "(type $f{index} (func (param {params}) (result {results})))"
        )?;
        writeln!(
            wat,
            "(func (export {:?}) (param {params}) (result {results})",
            function.name
        )?;
        for param in 0..function.params.len() {
            writeln!(wat, "local.get {param}")?;
        }
        writeln!(wat, "i32.const {index} call_indirect (type $f{index}))")?;
    }
    writeln!(
        wat,
        ") (core instance ${name}-forward (instantiate ${name}-forward))"
    )?;
    Ok(wat)
}

pub(crate) fn bind(name: &str, functions: &[Function]) -> Result<String> {
    let mut wat =
        format!("(core module ${name}-wire (import \"forward\" \"table\" (table 0 funcref))\n");
    for (index, function) in functions.iter().enumerate() {
        writeln!(
            wat,
            "(import \"targets\" {:?} (func $f{index} (param {}) (result {})))",
            function.name,
            function.params.join(" "),
            function.results.join(" ")
        )?;
    }
    wat.push_str("(elem (i32.const 0) func");
    for index in 0..functions.len() {
        write!(wat, " $f{index}")?;
    }
    writeln!(
        wat,
        "))\n(core instance ${name}-wire (instantiate ${name}-wire (with \"forward\" (instance ${name}-forward)) (with \"targets\" (instance"
    )?;
    for function in functions {
        writeln!(wat, "(export {:?} {})", function.name, function.target)?;
    }
    wat.push_str("))))\n");
    Ok(wat)
}
