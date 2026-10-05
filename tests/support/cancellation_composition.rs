//! Test-only composition of independently encoded application components.
use anyhow::Result;
pub(crate) fn compose(callee: &[u8], caller: &[u8]) -> Result<Vec<u8>> {
    compose_with_interface(callee, caller, None)
}
pub(crate) fn compose_with_interface(
    callee: &[u8],
    caller: &[u8],
    interface: Option<&str>,
) -> Result<Vec<u8>> {
    use wasm_encoder::{
        Alias, ComponentAliasSection, ComponentExportKind as Kind, ComponentExportSection,
        ComponentImportSection, ComponentInstanceSection, ComponentSectionId, ComponentTypeRef,
        ComponentTypeSection, PrimitiveValType, RawSection,
    };
    let mut component = wasm_encoder::Component::new();
    let mut types = ComponentTypeSection::new();
    types
        .function()
        .async_(true)
        .params([("mode", PrimitiveValType::U32)])
        .result(Some(PrimitiveValType::U32.into()));
    component.section(&types);
    let mut imports = ComponentImportSection::new();
    imports.import("wait", ComponentTypeRef::Func(0));
    component.section(&imports);
    for bytes in [callee, caller] {
        component.section(&RawSection {
            id: ComponentSectionId::Component.into(),
            data: bytes,
        });
    }
    let mut instances = ComponentInstanceSection::new();
    let callee_index = if let Some(interface) = interface {
        instances.export_items([("wait", Kind::Func, 0)]);
        instances.instantiate(0, [(interface, Kind::Instance, 0)]);
        1
    } else {
        instances.instantiate(0, [("wait", Kind::Func, 0)]);
        0
    };
    component.section(&instances);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: callee_index,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut instances = ComponentInstanceSection::new();
    instances.instantiate(1, [("wait", Kind::Func, 0), ("operation", Kind::Func, 1)]);
    component.section(&instances);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: callee_index + 1,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut exports = ComponentExportSection::new();
    exports.export("run", Kind::Func, 2, None);
    component.section(&exports);
    Ok(component.finish())
}
