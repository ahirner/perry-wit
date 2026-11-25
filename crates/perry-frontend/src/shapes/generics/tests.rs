use super::*;

#[test]
fn unproductive_alias_expansion_is_bounded() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let mut source = String::from("type Cycle = Cycle; type Fields = {a:number};");
    for index in 0..64 {
        let next = if index == 63 {
            "Fields".into()
        } else {
            format!("Chain{}", index + 1)
        };
        source.push_str(&format!("type Chain{index} = {next};"));
    }
    let ast = crate::source::parse(&source, "resolution.ts")?;
    let module = perry_hir::lower_module(&ast, "main", "resolution.ts")?;
    let ctx = Context::new();
    let foreign = crate::wit::WorldTypes::default();
    let decls = Declarations::new(
        &module.interfaces,
        &module.type_aliases,
        &module.imports,
        module.classes.iter().map(crate::shapes::ClassShape::Source),
        &foreign,
    );
    let mut types = TypeTable::new();
    let mut resolver = Resolver::new(&ctx, &decls, &mut types);
    for name in ["Cycle", "Chain0"] {
        assert_eq!(resolver.property(name, &[], "a"), None, "{name}");
    }
    Ok(())
}
