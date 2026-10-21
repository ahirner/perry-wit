//! Private unit futures for promise observers. The anchor exists while the component
//! encoder resolves canonical future builtins, and its synthetic export is removed afterwards.
use anyhow::{Context, Result, ensure};
use std::convert::Infallible;
use wasm_encoder::{
    reencode::{self, Reencode, ReencodeComponent},
    *,
};
use wasmparser::{Parser, Payload, TypeRef};

/// Name of the synthetic export whose `future` result type backs runtime promise signals.
/// The runtime imports the canonical future builtins under this name.
pub(crate) const ANCHOR: &str = "perry-internal-promise";

/// Whether the core module imports promise-signal future builtins and so needs the anchor.
pub(crate) fn uses_signals(core: &[u8]) -> Result<bool> {
    for payload in Parser::new(0).parse_all(core) {
        if let Payload::ImportSection(imports) = payload? {
            for import in imports.into_imports() {
                if import?.name.ends_with(ANCHOR) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// The core module and world extended by the anchor export: a trapping function returning
/// `future`, so the component encoder resolves the future builtins the runtime imports.
pub(crate) fn prepare(
    core: &[u8],
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<(Vec<u8>, wit_parser::Resolve)> {
    let mut resolve = resolve.clone();
    let key = wit_parser::WorldKey::Name(ANCHOR.into());
    ensure!(
        !resolve.worlds[world].exports.contains_key(&key),
        "reserved internal promise anchor"
    );
    let ty = resolve.types.alloc(wit_parser::TypeDef {
        name: None,
        kind: wit_parser::TypeDefKind::Future(None),
        owner: wit_parser::TypeOwner::None,
        docs: Default::default(),
        stability: Default::default(),
        span: Default::default(),
        external_id: None,
    });
    resolve.worlds[world].exports.insert(
        key,
        wit_parser::WorldItem::Function(wit_parser::Function {
            name: ANCHOR.into(),
            kind: wit_parser::FunctionKind::Freestanding,
            params: Vec::new(),
            result: Some(wit_parser::Type::Id(ty)),
            docs: Default::default(),
            stability: Default::default(),
            span: Default::default(),
            external_id: None,
        }),
    );
    let mut functions = 0;
    for payload in Parser::new(0).parse_all(core) {
        match payload? {
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    functions += u32::from(matches!(import?.ty, TypeRef::Func(_)));
                }
            }
            Payload::FunctionSection(section) => functions += section.count(),
            _ => {}
        }
    }
    let mut module = Module::new();
    Anchor {
        function: functions,
        ty: 0,
    }
    .parse_core_module(&mut module, Parser::new(0), core)?;
    Ok((module.finish(), resolve))
}

/// Re-encoder appending the anchor function (type, body and export) to a core module.
struct Anchor {
    /// Function index the appended anchor receives.
    function: u32,
    /// Type index of `() -> i32`, set once the type section is copied.
    ty: u32,
}
impl Reencode for Anchor {
    type Error = Infallible;
    fn parse_type_section(
        &mut self,
        types: &mut TypeSection,
        section: wasmparser::TypeSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_type_section(self, types, section)?;
        self.ty = types.len();
        types.ty().function([], [ValType::I32]);
        Ok(())
    }
    fn parse_function_section(
        &mut self,
        functions: &mut FunctionSection,
        section: wasmparser::FunctionSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_function_section(self, functions, section)?;
        functions.function(self.ty);
        Ok(())
    }
    fn parse_export_section(
        &mut self,
        exports: &mut ExportSection,
        section: wasmparser::ExportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_export_section(self, exports, section)?;
        exports.export(ANCHOR, ExportKind::Func, self.function);
        Ok(())
    }
    fn parse_code_section(
        &mut self,
        code: &mut CodeSection,
        section: wasmparser::CodeSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_code_section(self, code, section)?;
        let mut function = Function::new([]);
        function
            .instruction(&Instruction::Unreachable)
            .instruction(&Instruction::End);
        code.function(&function);
        Ok(())
    }
}

/// Re-encoder dropping the anchor's top-level component export and renumbering the
/// component functions after it.
struct Hide {
    /// Top-level component function index of the removed export.
    removed: u32,
    /// Component nesting depth. Only the outermost component is rewritten.
    depth: usize,
}
impl Reencode for Hide {
    type Error = Infallible;
}
impl ReencodeComponent for Hide {
    fn push_depth(&mut self) {
        self.depth += 1;
    }
    fn pop_depth(&mut self) {
        self.depth -= 1;
    }
    fn component_func_index(&mut self, index: u32) -> u32 {
        if self.depth == 0 && index > self.removed {
            index - 1
        } else {
            index
        }
    }
    fn parse_component_export(
        &mut self,
        exports: &mut ComponentExportSection,
        export: wasmparser::ComponentExport<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        if self.depth != 0 || export.name.name != ANCHOR {
            reencode::component_utils::parse_component_export(self, exports, export)?;
        }
        Ok(())
    }
}

/// Re-encode `component` after stripping the temporary anchor export added by [prepare].
pub(crate) fn hide(component: &[u8]) -> Result<Vec<u8>> {
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    let mut depth = 0;
    let mut removed = None;
    for payload in Parser::new(0).parse_all(component) {
        let payload = payload?;
        if matches!(payload, Payload::Version { .. }) {
            depth += 1;
        }
        if depth == 1
            && let Payload::ComponentExportSection(section) = &payload
        {
            let mut index = validator
                .types(0)
                .context("component exports precede the component's types")?
                .component_function_count();
            for export in section.clone() {
                let export = export?;
                if export.name.name == ANCHOR {
                    removed = Some(index);
                }
                index += u32::from(export.kind == wasmparser::ComponentExternalKind::Func);
            }
        }
        validator.payload(&payload)?;
        if matches!(payload, Payload::End(_)) {
            depth -= 1;
        }
    }
    let mut out = Component::new();
    Hide {
        removed: removed.context("component lacks the synthetic promise export")?,
        depth: 0,
    }
    .parse_component(&mut out, Parser::new(0), component)?;
    Ok(out.finish())
}
