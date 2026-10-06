use anyhow::{Context, Result};
use perry_wit::waffle_backend::WaffleCompiled;
use waffle::{FuncDecl, Module};

pub fn named_core(compiled: &WaffleCompiled) -> Result<Module<'static>> {
    let mut module = Module::from_wasm_bytes(&compiled.core, &Default::default())?;
    module.expand_all_funcs()?;
    // Linking preserves the order of core definitions, then appends helper bodies.
    let names = compiled
        .waffle_ir
        .lines()
        .filter(|line| line.starts_with("  func") && line.contains(" = #"))
        .map(|line| line.split('"').nth(1).context("missing function name"));
    let mut definitions = module
        .funcs
        .entries_mut()
        .filter_map(|(_, function)| match function {
            FuncDecl::Body(_, name, _) => Some(name),
            _ => None,
        });
    for original in names {
        let original = original?;
        if let Some(name) = definitions.next() {
            *name = original.into();
        }
    }
    drop(definitions);
    Ok(module.without_orig_bytes())
}
