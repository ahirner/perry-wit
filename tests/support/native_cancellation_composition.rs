use anyhow::Result;
/// Preserve the encoder's resolved import types when wrapping a component in a
/// test caller that explicitly cancels its `run` subtask.
pub(crate) fn compose(callee: &[u8], caller: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::{
        Alias, ComponentAliasSection, ComponentExportKind as Kind, ComponentExportSection,
        ComponentImportSection, ComponentInstanceSection, ComponentSectionId, ComponentTypeRef,
        ComponentTypeSection, PrimitiveValType, RawSection,
    };
    use wasmparser::{
        ComponentAlias, ComponentExternalKind, ComponentTypeRef as Ref, Parser, Payload,
    };
    let mut component = wasm_encoder::Component::new();
    let (mut types, mut functions, mut instances) = (0, 0, 0);
    let mut arguments = Vec::new();
    for payload in Parser::new(0).parse_all(callee) {
        let payload = payload?;
        match &payload {
            Payload::ModuleSection { .. } => break,
            Payload::Version { .. } | Payload::CustomSection(_) => continue,
            Payload::ComponentTypeSection(section) => types += section.count(),
            Payload::ComponentImportSection(section) => {
                for import in section.clone() {
                    let import = import?;
                    let (kind, index) = match import.ty {
                        Ref::Instance(_) => {
                            let index = instances;
                            instances += 1;
                            (Kind::Instance, index)
                        }
                        Ref::Func(_) => {
                            let index = functions;
                            functions += 1;
                            (Kind::Func, index)
                        }
                        _ => {
                            anyhow::bail!("Unsupported cancellation probe import: {:?}", import.ty)
                        }
                    };
                    arguments.push((import.name.name.to_string(), kind, index));
                }
            }
            Payload::ComponentAliasSection(section) => {
                for alias in section.clone() {
                    match alias? {
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Type,
                            ..
                        } => types += 1,
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Func,
                            ..
                        } => functions += 1,
                        alias => anyhow::bail!("Unsupported cancellation probe alias: {alias:?}"),
                    }
                }
            }
            _ => anyhow::bail!("Unexpected section before encoder core module"),
        }
        let (id, range) = payload.as_section().unwrap();
        component.section(&RawSection {
            id,
            data: &callee[usize::try_from(range.start)?..usize::try_from(range.end)?],
        });
    }
    let mut section = ComponentTypeSection::new();
    section
        .function()
        .async_(true)
        .params([("mode", PrimitiveValType::U32)])
        .result(Some(PrimitiveValType::U32.into()));
    component.section(&section);
    let mut imports = ComponentImportSection::new();
    imports.import("wait", ComponentTypeRef::Func(types));
    component.section(&imports);
    let wait = functions;
    for bytes in [callee, caller] {
        component.section(&RawSection {
            id: ComponentSectionId::Component.into(),
            data: bytes,
        });
    }
    let mut section = ComponentInstanceSection::new();
    section.instantiate(
        0,
        arguments
            .iter()
            .map(|(name, kind, index)| (name.as_str(), *kind, *index)),
    );
    component.section(&section);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: instances,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut section = ComponentInstanceSection::new();
    section.instantiate(
        1,
        [
            ("wait", Kind::Func, wait),
            ("operation", Kind::Func, wait + 1),
        ],
    );
    component.section(&section);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: instances + 1,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut exports = ComponentExportSection::new();
    exports.export("run", Kind::Func, wait + 2, None);
    component.section(&exports);
    Ok(component.finish())
}
